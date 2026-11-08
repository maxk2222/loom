//! Global tier-2 scheduler: metadata queue in Redis, atomic leases via Lua,
//! dispatch over client sessions, no-bypass fairness, watchdogs.

use crate::client_registry::ClientRegistry;
use crate::model_registry::ModelRegistry;
use crate::redisx::Rt;
use crate::router::{Router, Strategy};
use crate::session::SessionManager;
use loom_core::capability::ApiType;
use loom_core::config::SchedulerConfig;
use loom_core::ids::{LeaseId, RequestId};
use loom_core::RequiredCaps;
use loom_protocol::{frames, server_msgs};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum SchedErr {
    #[error("queue full")]
    QueueFull,
    #[error("no capacity")]
    NoCapacity,
    #[error("no candidates")]
    NoCandidates,
    #[error("client offline")]
    ClientOffline,
    /// Every candidate failed with an offline/unavailable state — structural,
    /// unlike `NoCapacity` (members exist and are merely busy). Queued
    /// requests in this state count against `max_attempts` so they cannot
    /// head-of-line block their class queue forever.
    #[error("all deployments unavailable")]
    AllUnavailable,
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("timeout")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
    #[error("backend: {0}")]
    Backend(String),
}

#[derive(Debug)]
pub struct PendingRequest {
    pub request_id: RequestId,
    pub api: ApiType,
    pub model: String,
    pub body: Vec<u8>,
    /// Request body media type ("" / "application/json" for JSON apis,
    /// "multipart/form-data; boundary=…" for audio transcriptions).
    pub content_type: String,
    pub required: RequiredCaps,
    pub project_id: String,
    pub prio: u32,
    pub enqueue_ts: i64,
    pub deadline_ts: i64,
    /// KV-cache affinity key (OpenAI `user` field): same key prefers the same
    /// member while the candidate set is stable — a hint, never a guarantee.
    pub sticky: String,
}

pub struct RequestOutcome {
    pub response_rx: mpsc::Receiver<Result<bytes::Bytes, SchedErr>>,
    pub end_rx: oneshot::Receiver<EndMeta>,
    pub routing_trace: Value,
    pub deployment_id: String,
    pub client_id: String,
    pub lease_id: String,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct EndMeta {
    pub status: String,
    pub error_code: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub ttft_ms: i64,
    pub duration_ms: i64,
}

struct PendingEntry {
    req: PendingRequest,
    done_tx: oneshot::Sender<Result<RequestOutcome, SchedErr>>,
}

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

struct Inner {
    rt: Rt,
    cfg: SchedulerConfig,
    sessions: Arc<SessionManager>,
    registry: ClientRegistry,
    models: ModelRegistry,
    router: Mutex<Router>,
    wake: Notify,
    pending: Mutex<HashMap<String, PendingEntry>>,
    /// request_id -> ack result channel (AssignAck from control stream).
    acks: Mutex<HashMap<String, oneshot::Sender<(bool, String)>>>,
    /// request_id -> RequestEnd channel.
    ends: Mutex<HashMap<String, oneshot::Sender<EndMeta>>>,
    /// request_id -> owning client + fencing (for cancel routing).
    active: Mutex<HashMap<String, (String, u64)>>,
}

impl Scheduler {
    pub fn new(
        rt: Rt,
        cfg: SchedulerConfig,
        sessions: Arc<SessionManager>,
        registry: ClientRegistry,
        models: ModelRegistry,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                rt,
                cfg,
                sessions,
                registry,
                models,
                router: Mutex::new(Router::new()),
                wake: Notify::new(),
                pending: Mutex::new(HashMap::new()),
                acks: Mutex::new(HashMap::new()),
                ends: Mutex::new(HashMap::new()),
                active: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// Access the shared Redis runtime handle (dashboard views).
    pub fn rt(&self) -> Rt {
        self.inner.rt.clone()
    }

    // ---- control-stream event sinks (called by grpc_service) ----

    pub async fn handle_assign_ack(&self, request_id: &str, accepted: bool, reason: &str) {
        if let Some(tx) = self.inner.acks.lock().await.remove(request_id) {
            let _ = tx.send((accepted, reason.to_string()));
        }
    }

    pub async fn complete_end(&self, request_id: &str, meta: EndMeta) {
        if let Some(tx) = self.inner.ends.lock().await.remove(request_id) {
            let _ = tx.send(meta);
        }
        self.inner.active.lock().await.remove(request_id);
        self.wake();
    }

    /// Drop a pending data-stream expectation so the awaiting dispatcher
    /// unblocks immediately (e.g. client went offline before opening it).
    pub async fn drop_expectation(&self, request_id: &str) {
        self.inner.sessions.drop_expectation(request_id).await;
    }

    /// Deliver an incoming client RequestData stream to the waiting dispatcher.
    pub async fn deliver_data(
        &self,
        request_id: &str,
        stream: tonic::Streaming<loom_protocol::pb::DataFrame>,
        outbound: mpsc::Sender<loom_protocol::pb::DataFrame>,
    ) -> bool {
        self.inner
            .sessions
            .deliver_data(request_id, stream, outbound)
            .await
    }

    /// Admin/caller abort: cancel if active (send CancelRequest to client),
    /// or drop from queue if still waiting.
    pub async fn cancel(&self, request_id: &str) -> bool {
        if let Some((client_id, fencing)) = self.inner.active.lock().await.get(request_id).cloned()
        {
            let _ = self
                .inner
                .sessions
                .send(
                    &client_id,
                    server_msgs::cancel(request_id, fencing, "aborted"),
                )
                .await;
            let _ = self
                .finish_pending(request_id, Err(SchedErr::Cancelled))
                .await;
            return true;
        }
        // queued?
        let removed = self.queue_remove(request_id).await.unwrap_or(false);
        if removed {
            let _ = self
                .finish_pending(request_id, Err(SchedErr::Cancelled))
                .await;
        }
        removed
    }

    async fn finish_pending(
        &self,
        request_id: &str,
        res: Result<RequestOutcome, SchedErr>,
    ) -> Result<(), ()> {
        if let Some(e) = self.inner.pending.lock().await.remove(request_id) {
            let _ = e.done_tx.send(res);
            return Ok(());
        }
        Err(())
    }

    async fn queue_remove(&self, request_id: &str) -> Result<bool, SchedErr> {
        let mut c = self.inner.rt.conn.clone();
        // prio unknown → try all class queues
        for p in 0..self.inner.cfg.priority_classes.max(1) {
            let res: String = redis::Script::new(crate::lua::QUEUE_REMOVE)
                .key(self.inner.rt.k(&format!("queue:{p}")))
                .key(self.inner.rt.k(&format!("req:{request_id}")))
                .key(self.inner.rt.k("req:watchdog"))
                .key(self.inner.rt.k("queue:count"))
                .arg(request_id)
                .invoke_async(&mut c)
                .await
                .map_err(|e| SchedErr::Backend(e.to_string()))?;
            if res == "OK" {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ---- main submit path ----

    pub async fn submit(&self, req: PendingRequest) -> Result<RequestOutcome, SchedErr> {
        if self.class_queue_empty(req.prio).await {
            match self.try_dispatch(&req).await {
                Ok(o) => return Ok(o),
                // AllUnavailable also queues: right after server restarts or
                // client reconnects the dep hash still reads `draining` until
                // the first capacity report lands — a brief queue beats an
                // instant 503. The dispatcher's attempt counter still bounds
                // genuinely-dead members to ~3 wake cycles.
                Err(SchedErr::NoCapacity | SchedErr::AllUnavailable) => {}
                Err(e) => return Err(e),
            }
        }
        self.enqueue_and_wait(req).await
    }

    async fn class_queue_empty(&self, prio: u32) -> bool {
        let mut c = self.inner.rt.conn.clone();
        let n: i64 = redis::cmd("ZCARD")
            .arg(self.inner.rt.k(&format!("queue:{prio}")))
            .query_async(&mut c)
            .await
            .unwrap_or(0);
        n == 0
    }

    async fn enqueue_and_wait(&self, req: PendingRequest) -> Result<RequestOutcome, SchedErr> {
        let rid = req.request_id.to_string();
        // Register pending BEFORE enqueueing — a dispatcher pop between the
        // enqueue script and the insert would otherwise drop the request.
        let (done_tx, done_rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .await
            .insert(rid.clone(), PendingEntry { req, done_tx });
        let mut c = self.inner.rt.conn.clone();
        // pull fields out under the guard, then drop it before the lua await
        let (prio, ets, proj, model, dl) = {
            let p = self.inner.pending.lock().await;
            let r = &p.get(&rid).unwrap().req;
            (
                r.prio,
                r.enqueue_ts,
                r.project_id.clone(),
                r.model.clone(),
                r.deadline_ts,
            )
        };
        let res: String = redis::Script::new(crate::lua::QUEUE_ENQUEUE)
            .key(self.inner.rt.k(&format!("queue:{prio}")))
            .key(self.inner.rt.k(&format!("req:{rid}")))
            .key(self.inner.rt.k("queue:count"))
            .key(self.inner.rt.k("req:watchdog"))
            .arg(&rid)
            .arg(ets)
            .arg(self.inner.cfg.max_queue as i64)
            .arg(&proj)
            .arg(&model)
            .arg(prio)
            .arg(dl)
            .invoke_async(&mut c)
            .await
            .map_err(|e| SchedErr::Backend(e.to_string()))?;
        if res != "OK" {
            self.inner.pending.lock().await.remove(&rid);
            return Err(SchedErr::QueueFull);
        }
        self.inner.wake.notify_one();
        let wait = self.inner.cfg.queue_timeout_secs.max(5);
        match tokio::time::timeout(Duration::from_secs(wait), done_rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(SchedErr::Cancelled),
            Err(_) => {
                // queue timeout: pull out + mark cancelled
                let _ = self.queue_remove(&rid).await;
                let _ = self.finish_pending(&rid, Err(SchedErr::Timeout)).await;
                Err(SchedErr::Timeout)
            }
        }
    }

    pub fn spawn_dispatch_loop(&self) {
        let sched = self.clone();
        tokio::spawn(async move {
            tracing::info!("dispatch loop started");
            loop {
                sched.inner.wake.notified().await;
                tracing::debug!("dispatch wake fired");
                sched.dispatch_pending().await;
            }
        });
        // Watchdog: sweep expired reserved leases + queued-request deadlines.
        let sched2 = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                sched2.watchdog_pass().await;
            }
        });
    }

    /// 1) Expired reserved leases → free slot + nudge dispatcher to retry.
    /// 2) Queued requests past their deadline → remove + fail with timeout.
    async fn watchdog_pass(&self) {
        let now = crate::client_registry::now_ms();
        // a) expired queue entries (deadline score in req:watchdog)
        let mut c = self.inner.rt.conn.clone();
        let expired: Vec<String> = redis::cmd("ZRANGEBYSCORE")
            .arg(self.inner.rt.k("req:watchdog"))
            .arg(0)
            .arg(now)
            .query_async(&mut c)
            .await
            .unwrap_or_default();
        for rid in expired {
            let removed = self.queue_remove(&rid).await.unwrap_or(false);
            if removed {
                let _ = self.finish_pending(&rid, Err(SchedErr::Timeout)).await;
            }
        }
        // b) expired reserved leases on all known deployments
        let dep_ids: Vec<String> =
            sqlx::query_scalar("SELECT id::text FROM deployments WHERE enabled = true")
                .fetch_all(&self.inner.registry.db)
                .await
                .unwrap_or_default();
        for dep in dep_ids {
            let swept: Vec<String> = redis::Script::new(crate::lua::LEASE_SWEEP)
                .key(self.inner.rt.k(&format!("dep:{dep}")))
                .key(self.inner.rt.k(&format!("dep:{dep}:leases")))
                .arg(now)
                .invoke_async(&mut c)
                .await
                .unwrap_or_default();
            for rid in swept {
                // reserved lease died → dispatcher retries next candidate
                self.handle_assign_ack(&rid, false, "lease_expired").await;
            }
        }
    }

    async fn dispatch_pending(&self) {
        for _ in 0..128 {
            let mut c = self.inner.rt.conn.clone();
            let keys: Vec<String> = (0..self.inner.cfg.priority_classes.max(1))
                .map(|p| self.inner.rt.k(&format!("queue:{p}")))
                .collect();
            let popped: Result<Option<String>, _> = redis::Script::new(crate::lua::QUEUE_POP)
                .key(keys)
                .invoke_async(&mut c)
                .await;
            match &popped {
                Err(e) => tracing::warn!(error=%e, "queue_pop failed"),
                Ok(v) => tracing::debug!(popped=?v, "queue_pop result"),
            }
            let Ok(Some(rid)) = popped else { return };
            let _: () = redis::pipe()
                .cmd("DECR")
                .arg(self.inner.rt.k("queue:count"))
                .ignore()
                .cmd("ZREM")
                .arg(self.inner.rt.k("req:watchdog"))
                .arg(&rid)
                .ignore()
                .query_async(&mut c)
                .await
                .unwrap_or(());
            let entry = self.inner.pending.lock().await.remove(&rid);
            let Some(e) = entry else { continue };
            let outcome = tokio::time::timeout(
                Duration::from_millis(self.inner.cfg.assign_ack_timeout_ms.max(5000) + 30_000),
                self.try_dispatch(&e.req),
            )
            .await
            .map_err(|_| SchedErr::Timeout)
            .and_then(|r| r);
            match outcome {
                Ok(outcome) => {
                    let _ = e.done_tx.send(Ok(outcome));
                }
                Err(SchedErr::NoCapacity) => {
                    // members busy but reachable: requeue preserving FIFO and
                    // stop — popping further entries would only churn.
                    if self.requeue(e, &rid, false).await {
                        return;
                    }
                    continue;
                }
                Err(SchedErr::AllUnavailable) => {
                    // Grace window: freshly enqueued requests (<5s) are
                    // likely seeing a reconnect/report gap — requeue uncounted
                    // and stop, so a wake (capacity report) retries them
                    // instead of burning all 3 attempts within one burst.
                    // Older requests count attempts normally and die bounded.
                    let fresh = crate::client_registry::now_ms() - e.req.enqueue_ts < 5_000;
                    if self.requeue(e, &rid, !fresh).await && fresh {
                        return;
                    }
                    continue;
                }
                Err(err) => {
                    // NoCandidates is structural (no member can serve this
                    // API/caps) — fail fast instead of requeueing for the
                    // full queue timeout.
                    let _ = e.done_tx.send(Err(err));
                }
            }
        }
    }

    /// Requeue a popped entry back into its class queue. Returns true when the
    /// entry was requeued (stop dispatching — capacity exhausted); false when
    /// the entry was finally failed (caller already notified). `count` marks
    /// the pop as a spent attempt — only structural shortages count, so a
    /// request queued behind merely-busy members waits for its deadline
    /// instead of dying after a few head visits.
    async fn requeue(&self, e: PendingEntry, rid: &str, count: bool) -> bool {
        let PendingEntry { req, done_tx } = e;
        let mut c = self.inner.rt.conn.clone();
        let attempts: u32 = if count {
            redis::cmd("HINCRBY")
                .arg(self.inner.rt.k(&format!("req:{rid}")))
                .arg("attempts")
                .arg(1)
                .query_async(&mut c)
                .await
                .unwrap_or(0)
        } else {
            0
        };
        if count && attempts >= self.inner.cfg.max_attempts {
            let _ = done_tx.send(Err(SchedErr::AllUnavailable));
            return false;
        }
        // pending entry back first, then enqueue (no drop race)
        let (prio, ets, proj, model, dl) = (
            req.prio,
            req.enqueue_ts,
            req.project_id.clone(),
            req.model.clone(),
            req.deadline_ts,
        );
        self.inner
            .pending
            .lock()
            .await
            .insert(rid.to_string(), PendingEntry { req, done_tx });
        let res: String = redis::Script::new(crate::lua::QUEUE_ENQUEUE)
            .key(self.inner.rt.k(&format!("queue:{prio}")))
            .key(self.inner.rt.k(&format!("req:{rid}")))
            .key(self.inner.rt.k("queue:count"))
            .key(self.inner.rt.k("req:watchdog"))
            .arg(rid)
            .arg(ets) // original ts → FIFO preserved
            .arg(self.inner.cfg.max_queue as i64)
            .arg(&proj)
            .arg(&model)
            .arg(prio)
            .arg(dl)
            .invoke_async(&mut c)
            .await
            .unwrap_or_else(|_| "ERR".into());
        if res != "OK" {
            // enqueue failed (queue full while retrying) → fail the caller
            if let Some(e) = self.inner.pending.lock().await.remove(rid) {
                let _ = e.done_tx.send(Err(SchedErr::QueueFull));
            }
            return false;
        }
        true
    }

    async fn try_dispatch(&self, req: &PendingRequest) -> Result<RequestOutcome, SchedErr> {
        let Some(vm) = self.inner.models.resolve(&req.model).await else {
            return Err(SchedErr::NoCandidates);
        };
        let mut candidates = self
            .inner
            .models
            .candidates(&vm, req.api, &req.required)
            .await;
        let mut vm_name = vm.api_name.clone();
        if candidates.is_empty() {
            if let Some(fb) = self.inner.models.fallback(&vm).await {
                candidates = self
                    .inner
                    .models
                    .candidates(&fb, req.api, &req.required)
                    .await;
                vm_name = fb.api_name;
            }
        }
        if candidates.is_empty() {
            return Err(SchedErr::NoCandidates);
        }
        let strategy = Strategy::parse(
            &self
                .inner
                .models
                .resolve(&vm_name)
                .await
                .map(|v| v.routing_strategy)
                .unwrap_or_default(),
        );
        let runtime = self.runtime_view(&candidates).await;
        let decision = {
            let mut r = self.inner.router.lock().await;
            r.route(&vm_name, strategy, candidates, &runtime, &req.required)
        };
        let mut trace = decision.trace;
        let mut ranked = decision.ranked;
        // KV-cache affinity: a caller-supplied `user` key picks one member
        // deterministically while the set is stable — the hash target is
        // rotated to the front, all other candidates remain as fallbacks.
        if !req.sticky.is_empty() && ranked.len() > 1 {
            // Hash onto the *sorted member set*: `ranked` order varies per
            // call (weighted/least_in_flight), so indexing it directly would
            // pick a different member every time. The identity choice only
            // shifts when the candidate set itself changes.
            let mut ids: Vec<&str> = ranked.iter().map(|m| m.deployment_id.as_str()).collect();
            ids.sort_unstable();
            let chosen = ids[(fnv1a(&req.sticky) as usize) % ids.len()];
            if let Some(pos) = ranked.iter().position(|m| m.deployment_id == chosen) {
                tracing::debug!(req = %req.request_id, dep = %chosen, "sticky affinity member");
                ranked.swap(0, pos);
            }
        }
        // `NoCapacity`/`Rejected` mean a member exists and could serve once it
        // frees up — transient. `ClientOffline` means unavailable; only when
        // every candidate is unreachable is the shortage structural.
        let mut saw_transient = false;
        for member in ranked {
            match self.lease_and_assign(req, &member).await {
                Ok(outcome) => {
                    trace["deployment_id"] = json!(member.deployment_id);
                    trace["client_id"] = json!(member.client_id);
                    return Ok(RequestOutcome {
                        routing_trace: trace,
                        ..outcome
                    });
                }
                Err(e @ (SchedErr::NoCapacity | SchedErr::Rejected(_))) => {
                    saw_transient = true;
                    tracing::debug!(req = %req.request_id, dep = %member.deployment_id, err = %e, "candidate failed, trying next");
                    continue;
                }
                Err(e @ SchedErr::ClientOffline) => {
                    tracing::debug!(req = %req.request_id, dep = %member.deployment_id, err = %e, "candidate failed, trying next");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(req = %req.request_id, dep = %member.deployment_id, error = %e, "dispatch error");
                    return Err(e);
                }
            }
        }
        Err(if saw_transient {
            SchedErr::NoCapacity
        } else {
            SchedErr::AllUnavailable
        })
    }

    async fn runtime_view(
        &self,
        candidates: &[crate::model_registry::Member],
    ) -> HashMap<String, (u32, String)> {
        let mut out = HashMap::new();
        for m in candidates {
            if let Ok(Some(rt)) = self.inner.registry.dep_runtime(&m.deployment_id).await {
                out.insert(m.deployment_id.clone(), (rt.tier2_free, rt.state));
            }
        }
        out
    }

    async fn lease_and_assign(
        &self,
        req: &PendingRequest,
        member: &crate::model_registry::Member,
    ) -> Result<RequestOutcome, SchedErr> {
        let rid = req.request_id.to_string();
        let lease_id = LeaseId::new().to_string();
        let dep_id = member.deployment_id.clone();
        let client_id = member.client_id.clone();
        let mut c = self.inner.rt.conn.clone();

        let res: redis::Value = redis::Script::new(crate::lua::LEASE_ACQUIRE)
            .key(self.inner.rt.k(&format!("dep:{dep_id}")))
            .key(self.inner.rt.k(&format!("dep:{dep_id}:leases")))
            .key(self.inner.rt.k(&format!("lease:{lease_id}")))
            .key(self.inner.rt.k("cap:fencing"))
            .arg(&rid)
            .arg(&client_id)
            .arg(&dep_id)
            .arg(self.inner.cfg.lease_ttl_secs as i64 * 1000)
            .arg(crate::client_registry::now_ms())
            .arg(0)
            .invoke_async(&mut c)
            .await
            .map_err(|e| SchedErr::Backend(e.to_string()))?;
        let fencing = match &res {
            redis::Value::Array(v) => match v.first() {
                Some(redis::Value::Int(i)) => *i as u64,
                _ => 0,
            },
            redis::Value::SimpleString(s) if s == "NO_CAPACITY" => {
                return Err(SchedErr::NoCapacity)
            }
            redis::Value::SimpleString(s) if s == "UNAVAILABLE" => {
                return Err(SchedErr::ClientOffline)
            }
            _ => 0,
        };
        if fencing == 0 {
            return Err(SchedErr::NoCapacity);
        }

        // Expectations BEFORE AssignRequest (race-free).
        let data_rx = self.inner.sessions.expect_data(&rid).await;
        let (ack_tx, ack_rx) = oneshot::channel();
        self.inner.acks.lock().await.insert(rid.clone(), ack_tx);
        let end_rx = {
            let (tx, rx) = oneshot::channel();
            self.inner.ends.lock().await.insert(rid.clone(), tx);
            rx
        };

        let assign = server_msgs::assign(
            &rid,
            &lease_id,
            fencing,
            req.deadline_ts,
            req.api.as_str(),
            loom_protocol::pb::RequiredCaps {
                tools: req.required.tools,
                vision: req.required.vision,
                streaming: req.required.streaming,
                json_mode: req.required.json_mode,
            },
            req.body.len() as u32,
            &req.model,
            &dep_id,
            &req.content_type,
        );
        if self.inner.sessions.send(&client_id, assign).await.is_err() {
            self.cleanup_assign(&rid, &lease_id, fencing, &dep_id).await;
            return Err(SchedErr::ClientOffline);
        }

        // Await AssignAck (bounded).
        let ack = tokio::time::timeout(
            Duration::from_millis(self.inner.cfg.assign_ack_timeout_ms),
            ack_rx,
        )
        .await;
        match ack {
            Ok(Ok((true, _))) => {}
            Ok(Ok((false, reason))) => {
                self.cleanup_assign(&rid, &lease_id, fencing, &dep_id).await;
                return Err(SchedErr::Rejected(reason));
            }
            _ => {
                self.cleanup_assign(&rid, &lease_id, fencing, &dep_id).await;
                return Err(SchedErr::ClientOffline);
            }
        }

        // Confirm lease → then await client data stream.
        let _: String = redis::Script::new(crate::lua::LEASE_CONFIRM)
            .key(self.inner.rt.k(&format!("lease:{lease_id}")))
            .key(self.inner.rt.k(&format!("dep:{dep_id}")))
            .arg(fencing)
            .invoke_async(&mut c)
            .await
            .unwrap_or_default();

        // activate lease state
        let _: String = redis::Script::new(crate::lua::LEASE_ACTIVATE)
            .key(self.inner.rt.k(&format!("lease:{lease_id}")))
            .arg(fencing)
            .invoke_async(&mut c)
            .await
            .unwrap_or_default();

        let (mut inbound, outbound) =
            match tokio::time::timeout(Duration::from_secs(10), data_rx).await {
                Ok(Ok(pair)) => pair,
                _ => {
                    self.cleanup_assign(&rid, &lease_id, fencing, &dep_id).await;
                    return Err(SchedErr::ClientOffline);
                }
            };

        self.inner
            .active
            .lock()
            .await
            .insert(rid.clone(), (client_id.clone(), fencing));

        // Bridge task: send request payload via outbound frames, read
        // response_chunk frames from inbound into resp channel.
        let (resp_tx, resp_rx) = mpsc::channel::<Result<bytes::Bytes, SchedErr>>(32);
        let cancel = CancellationToken::new();
        {
            let body = req.body.clone();
            let cancel2 = cancel.clone();
            let rt = self.inner.rt.clone();
            let lease2 = lease_id.clone();
            let dep2 = dep_id.clone();
            let rid_for_release = rid.clone();
            tokio::spawn(async move {
                use futures_util::StreamExt;
                // 1) deliver payload to client (chunked)
                for chunk in body.chunks(256 * 1024) {
                    if outbound
                        .send(frames::request_json(chunk.to_vec()))
                        .await
                        .is_err()
                    {
                        let _ = resp_tx
                            .send(Err(SchedErr::Backend("data stream closed".into())))
                            .await;
                        return;
                    }
                }
                // 2) forward client response chunks until End/Error/cancel
                tracing::debug!(req = %rid_for_release, "bridge: payload sent, reading inbound");
                loop {
                    let next = tokio::select! {
                        _ = cancel2.cancelled() => break,
                        v = inbound.next() => v,
                    };
                    tracing::debug!(req = %rid_for_release, frame = ?next.as_ref().map(|r| r.as_ref().map(|f| std::mem::discriminant(&f.frame))), "bridge inbound");
                    match next {
                        Some(Ok(loom_protocol::pb::DataFrame {
                            frame: Some(loom_protocol::pb::data_frame::Frame::ResponseChunk(b)),
                        })) => {
                            tracing::debug!(req = %rid_for_release, n = b.len(), "response_chunk");
                            if resp_tx.send(Ok(b.into())).await.is_err() {
                                // caller gone: cancel upstream by closing outbound
                                break;
                            }
                        }
                        Some(Ok(loom_protocol::pb::DataFrame {
                            frame: Some(loom_protocol::pb::data_frame::Frame::Error(e)),
                        })) => {
                            let _ = resp_tx.send(Err(SchedErr::Backend(e.message))).await;
                            break;
                        }
                        Some(Ok(loom_protocol::pb::DataFrame {
                            frame: Some(loom_protocol::pb::data_frame::Frame::End(_)),
                        })) => {
                            tracing::debug!(req = %rid_for_release, "end frame");
                            break;
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => {
                            let _ = resp_tx.send(Err(SchedErr::Backend(e.to_string()))).await;
                            break;
                        }
                        None => break,
                    }
                }
                // Release the lease — slot freed exactly once, idempotent.
                let mut c = rt.conn.clone();
                let _: String = redis::Script::new(crate::lua::LEASE_RELEASE)
                    .key(rt.k(&format!("lease:{lease2}")))
                    .key(rt.k(&format!("dep:{dep2}")))
                    .key(rt.k(&format!("dep:{dep2}:leases")))
                    .arg(fencing)
                    .arg(&rid_for_release)
                    .invoke_async(&mut c)
                    .await
                    .unwrap_or_default();
            });
        }
        Ok(RequestOutcome {
            response_rx: resp_rx,
            end_rx,
            routing_trace: json!({}),
            deployment_id: dep_id,
            client_id,
            lease_id,
            cancel,
        })
    }

    async fn cleanup_assign(&self, rid: &str, lease_id: &str, fencing: u64, dep_id: &str) {
        self.inner.sessions.drop_expectation(rid).await;
        self.inner.acks.lock().await.remove(rid);
        self.inner.ends.lock().await.remove(rid);
        let mut c = self.inner.rt.conn.clone();
        let _: String = redis::Script::new(crate::lua::LEASE_RELEASE)
            .key(self.inner.rt.k(&format!("lease:{lease_id}")))
            .key(self.inner.rt.k(&format!("dep:{dep_id}")))
            .key(self.inner.rt.k(&format!("dep:{dep_id}:leases")))
            .arg(fencing)
            .arg(rid)
            .invoke_async(&mut c)
            .await
            .unwrap_or_default();
    }

    /// Register the outbound half of a RequestData stream (grpc_service side).
    /// The bridge task uses it to send request_json frames to the client.
    pub async fn attach_data_outbound(
        &self,
        _rid: &str,
        _tx: mpsc::Sender<Result<loom_protocol::pb::DataFrame, tonic::Status>>,
    ) {
    }
}

/// FNV-1a 64-bit — stable across platforms, cheap, good enough for hashing a
/// short affinity key onto a candidate index.
fn fnv1a(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
