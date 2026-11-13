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
    /// Request footprint exceeds every candidate's KV budget — a structural
    /// "will never fit" verdict that must fail fast instead of queueing.
    #[error("request footprint exceeds every deployment budget")]
    ContextTooLarge,
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
    /// Reserved KV tokens for budget admission (0 = non-LLM api / unset).
    /// Computed once in the gateway; travels through queue/requeue unchanged.
    pub footprint_tokens: u64,
}

pub struct RequestOutcome {
    pub response_rx: mpsc::Receiver<Result<bytes::Bytes, SchedErr>>,
    /// HTTP status reported by the client in the data-stream `End` frame
    /// (0 = unknown/older client).
    pub upstream_status: std::sync::Arc<std::sync::atomic::AtomicU32>,
    pub end_rx: oneshot::Receiver<EndMeta>,
    pub routing_trace: Value,
    pub deployment_id: String,
    pub client_id: String,
    pub lease_id: String,
    pub cancel: CancellationToken,
    /// Milliseconds between gateway enqueue and successful member dispatch —
    /// includes queue wait, near-zero when dispatched immediately.
    pub queue_wait_ms: i64,
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
    /// request_id -> (client_id, fencing, lease_id, dep_id) — cancel routing
    /// plus mid-flight lease footprint updates.
    active: Mutex<HashMap<String, (String, u64, String, String)>>,
    /// This server instance's identity — stamped on queued entries so a
    /// second server sharing the Redis namespace can tell "orphaned entry"
    /// (owner dead/unknown) from "entry owned by a live peer" (requeue it
    /// back instead of dropping the caller's request).
    instance_id: String,
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
                instance_id: loom_core::ids::opaque_token(12),
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

    /// Live (queued, in-flight) request counts for the dashboard overview.
    /// History only stores terminal states, so it can't supply these.
    /// `queued` is the Redis queue gauge; `active` is the in-process map of
    /// dispatched requests — `pending` only ever holds *queued* entries, so
    /// subtracting queued from it undercounts in-flight to ~0.
    pub async fn live_counts(&self) -> (i64, i64) {
        let mut c = self.inner.rt.conn.clone();
        let queued: i64 = redis::cmd("GET")
            .arg(self.inner.rt.k("queue:count"))
            .query_async(&mut c)
            .await
            .unwrap_or(0);
        let active = self.inner.active.lock().await.len() as i64;
        (queued, active)
    }

    /// Number of configured queue priority classes (≥1). Valid class ids are
    /// `0..classes-1`; class 0 is the highest (popped first).
    pub fn priority_classes(&self) -> u32 {
        self.inner.cfg.priority_classes.max(1)
    }

    /// Queue depth per class (`0..classes-1`, index 0 = highest). For the
    /// per-class dashboard/metrics view — `live_counts` only exposes the
    /// global gauge.
    pub async fn class_depths(&self) -> Vec<i64> {
        let mut c = self.inner.rt.conn.clone();
        let mut pipe = redis::pipe();
        for p in 0..self.priority_classes() {
            pipe.cmd("ZCARD")
                .arg(self.inner.rt.k(&format!("queue:{p}")));
        }
        pipe.query_async(&mut c).await.unwrap_or_default()
    }

    /// Does `model` currently have at least one dispatchable member?
    /// Combines the snapshot filter (enabled + capabilities for `api` and
    /// `required`) with runtime liveness — a member whose client is gone or
    /// whose dep was marked unavailable is dead weight for routing.
    /// `available` and `draining` members count as live: requests may queue
    /// behind a busy member and a draining member reopens once it recovers.
    /// Used by the auto-router to keep dead members unpickable.
    pub async fn serviceable(&self, model: &str, api: ApiType, required: &RequiredCaps) -> bool {
        let Some(vm) = self.inner.models.resolve(model).await else {
            return false;
        };
        let candidates = self.inner.models.candidates(&vm, api, required).await;
        for m in &candidates {
            if let Ok(Some(rt)) = self.inner.registry.dep_runtime(&m.deployment_id).await {
                // `down` health (crash breaker open or dead backend) excludes
                // a member even while its admission state still reads
                // available/draining — the generation path is dead.
                if (rt.state == "available" || rt.state == "draining") && rt.health != "down" {
                    return true;
                }
            }
        }
        false
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

    /// KV tokens a request of this api/body reserves under the deployment
    /// budget. Only generative text apis grow KV state; embeddings and
    /// media get a flat bookkeeping footprint so admission still counts them.
    pub fn footprint(&self, api: loom_core::capability::ApiType, body: &serde_json::Value) -> u64 {
        use loom_core::capability::ApiType;
        match api {
            ApiType::Chat | ApiType::Completions => loom_core::tokens::request_footprint(
                body,
                self.inner.cfg.kv_output_reserve_tokens,
                self.inner.cfg.kv_margin_pct,
            ),
            _ => loom_core::tokens::flat_footprint(),
        }
    }

    /// Mid-flight footprint correction: a client's cumulative token usage
    /// report converges the lease reservation toward real KV consumption —
    /// the submit-time estimate is replaced by truth so the budget gate
    /// tracks reality during long decodes. Monotonic on the lease side.
    pub async fn update_lease_fp(&self, request_id: &str, tokens_used: u64) {
        let Some((_, fencing, lease_id, dep_id)) =
            self.inner.active.lock().await.get(request_id).cloned()
        else {
            return;
        };
        let mut c = self.inner.rt.conn.clone();
        let _: Result<redis::Value, _> = redis::Script::new(crate::lua::LEASE_FP_UPDATE)
            .key(self.inner.rt.k(&format!("lease:{lease_id}")))
            .key(self.inner.rt.k(&format!("dep:{dep_id}")))
            .arg(fencing)
            .arg(tokens_used.min(i64::MAX as u64) as i64)
            .invoke_async(&mut c)
            .await;
    }

    /// Deliver an incoming client RequestData stream to the waiting dispatcher.
    pub async fn deliver_data(
        &self,
        request_id: &str,
        stream: std::pin::Pin<
            Box<
                dyn futures_util::Stream<Item = Result<loom_protocol::pb::DataFrame, tonic::Status>>
                    + Send,
            >,
        >,
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
        if let Some((client_id, fencing, _, _)) =
            self.inner.active.lock().await.get(request_id).cloned()
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

    /// Mirror the Redis `queue:count` gauge into the metrics exporter. Cheap
    /// single GET — called on enqueue and once per dispatch pass so the
    /// exported depth tracks every mutation path (enqueue/pop/remove/expiry).
    /// Also publishes the per-class depths (one pipelined ZCARD round-trip).
    async fn sync_queue_depth(&self) {
        let mut c = self.inner.rt.conn.clone();
        let n: Option<i64> = redis::cmd("GET")
            .arg(self.inner.rt.k("queue:count"))
            .query_async(&mut c)
            .await
            .unwrap_or_default();
        crate::metrics::queue_len(n.unwrap_or(0) as f64);
        for (p, d) in self.class_depths().await.iter().enumerate() {
            crate::metrics::queue_len_class(p as u32, *d as f64);
        }
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

    pub async fn submit(&self, mut req: PendingRequest) -> Result<RequestOutcome, SchedErr> {
        // Authoritative class clamp: an out-of-range class would land in a
        // queue nothing ever pops (classes are `0..priority_classes`), where
        // the request would sit until deadline. Covers every submit path —
        // DB-derived key/project classes and router-internal submits alike.
        req.prio = clamp_class(req.prio, self.priority_classes());
        if self.higher_or_equal_empty(req.prio).await {
            let deadline = std::time::Instant::now()
                + Duration::from_millis(self.inner.cfg.assign_ack_timeout_ms.max(5000) + 30_000);
            match self.try_dispatch(&req, deadline).await {
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

    /// Strict-priority fast-path guard: a request may only skip the queue when
    /// no higher-or-equal class has waiting work. Checking only the request's
    /// own class would let low-priority submits race ahead of queued
    /// high-priority entries and steal freshly-freed capacity. Queued
    /// *lower*-class work never blocks a higher-class submit — it may still
    /// dispatch first, which is exactly strict ordering.
    /// Fails closed (treated as non-empty) when Redis errors — enqueueing is
    /// always the safe path.
    async fn higher_or_equal_empty(&self, prio: u32) -> bool {
        let mut c = self.inner.rt.conn.clone();
        let keys: Vec<String> = (0..=prio)
            .map(|p| self.inner.rt.k(&format!("queue:{p}")))
            .collect();
        let n: i64 = redis::Script::new(crate::lua::QUEUE_ANY_NONEMPTY)
            .key(keys)
            .invoke_async(&mut c)
            .await
            .unwrap_or(1);
        n == 0
    }

    async fn enqueue_and_wait(&self, req: PendingRequest) -> Result<RequestOutcome, SchedErr> {
        let rid = req.request_id.to_string();
        // Register pending BEFORE enqueueing — a dispatcher pop between the
        // enqueue script and the insert would otherwise drop the request.
        let (prio, ets, proj, model, dl) = (
            req.prio,
            req.enqueue_ts,
            req.project_id.clone(),
            req.model.clone(),
            req.deadline_ts,
        );
        let (done_tx, done_rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .await
            .insert(rid.clone(), PendingEntry { req, done_tx });
        let mut c = self.inner.rt.conn.clone();
        let res: Result<String, _> = redis::Script::new(crate::lua::QUEUE_ENQUEUE)
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
            .arg(&self.inner.instance_id)
            .invoke_async(&mut c)
            .await;
        let res = match res {
            Ok(res) => res,
            Err(e) => {
                // A script error leaves no queue member to trigger the
                // dead-waiter cleanup — drop the pending entry (which holds
                // the whole request body) ourselves.
                self.inner.pending.lock().await.remove(&rid);
                return Err(SchedErr::Backend(e.to_string()));
            }
        };
        if res != "OK" {
            self.inner.pending.lock().await.remove(&rid);
            return Err(SchedErr::QueueFull);
        }
        self.sync_queue_depth().await;
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
            // Periodic sweep alongside event wakes: capacity reports are
            // change-driven, so without other traffic nothing re-arms a
            // queued request — it would sit the full queue timeout even
            // when all members are permanently unavailable. The 2s tick
            // guarantees attempt counting progresses and busy members get
            // retried promptly.
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = sched.inner.wake.notified() => {
                        tracing::debug!("dispatch wake fired");
                    }
                    _ = tick.tick() => {}
                }
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
        // a2) `queue:count` is maintained inside the queue scripts, but any
        // missed decrement (legacy data, script error) re-arms QueueFull
        // forever — periodically clamp it to the sum of the class zsets.
        // The measure+fix itself must be atomic: a read-then-SET from here
        // would race live traffic and inject the drift it corrects.
        {
            let mut keys: Vec<String> = (0..self.inner.cfg.priority_classes.max(1))
                .map(|p| self.inner.rt.k(&format!("queue:{p}")))
                .collect();
            keys.push(self.inner.rt.k("queue:count"));
            let (cur, n): (i64, i64) = redis::Script::new(crate::lua::QUEUE_COUNT_RECONCILE)
                .key(keys)
                .invoke_async::<Vec<i64>>(&mut c)
                .await
                .map(|v| (*v.first().unwrap_or(&0), *v.get(1).unwrap_or(&0)))
                .unwrap_or((0, 0));
            if cur != n {
                tracing::warn!(stored = cur, actual = n, "queue:count drift corrected");
            }
        }
        // b) expired reserved leases on ALL deployments — a disabled dep keeps
        // its lease set, and skipping it would leak the slots it still holds.
        let deps: Vec<(String, Option<i64>)> = sqlx::query_as(
            "SELECT id::text, COALESCE(kv_budget_tokens, reported_kv_budget_tokens) \
             FROM deployments",
        )
        .fetch_all(&self.inner.registry.db)
        .await
        .unwrap_or_default();
        for (dep, kv_budget) in deps {
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
            // KV accounting gauges — exported on the same tick so Grafana
            // tracks budget consumption next to slot pressure.
            let kv_used: Option<i64> = redis::cmd("HGET")
                .arg(self.inner.rt.k(&format!("dep:{dep}")))
                .arg("kv_used")
                .query_async(&mut c)
                .await
                .unwrap_or(None);
            crate::metrics::dep_kv(
                &dep,
                kv_used.unwrap_or(0) as f64,
                kv_budget.unwrap_or(0) as f64,
            );
            // c) active leases past the stream idle timeout → free slot +
            // fail the request (backend hang would otherwise burn the slot).
            let deadline = self.inner.cfg.stream_idle_timeout_secs as i64 * 1000;
            let timed_out: Vec<String> = redis::Script::new(crate::lua::ACTIVE_LEASE_SWEEP)
                .key(self.inner.rt.k(&format!("dep:{dep}:leases")))
                .key(self.inner.rt.k(&format!("dep:{dep}")))
                .arg(now)
                .arg(deadline)
                .invoke_async(&mut c)
                .await
                .unwrap_or_default();
            for rid in timed_out {
                // Tell the owning client to abort too — the request is dead
                // server-side but its backend call may still be running.
                if let Some((client_id, fencing, _, _)) =
                    self.inner.active.lock().await.get(&rid).cloned()
                {
                    let _ = self
                        .inner
                        .sessions
                        .send(
                            &client_id,
                            server_msgs::cancel(&rid, fencing, "request_deadline"),
                        )
                        .await;
                }
                self.complete_end(
                    &rid,
                    EndMeta {
                        status: "failed".into(),
                        error_code: "request_deadline".into(),
                        ..Default::default()
                    },
                )
                .await;
            }
        }
        // d) Reconcile the in-process maps: an `active`/`ends` entry whose
        // lease no longer exists in Redis is post-mortem (the bridge's grace
        // end should have fired, but a panicked task or a stale process must
        // not leave the finalizer hanging and the inflight slot held).
        {
            let act = self.inner.active.lock().await.clone();
            for (rid, (_cid, _f, lease_id, _d)) in act {
                let exists: bool = redis::cmd("EXISTS")
                    .arg(self.inner.rt.k(&format!("lease:{lease_id}")))
                    .query_async(&mut c)
                    .await
                    .unwrap_or(true);
                if !exists {
                    tracing::warn!(req = %rid, lease = %lease_id, "active entry with dead lease — reconciling");
                    self.complete_end(
                        &rid,
                        EndMeta {
                            status: "failed".into(),
                            error_code: "lease_lost".into(),
                            ..Default::default()
                        },
                    )
                    .await;
                }
            }
        }
    }

    async fn dispatch_pending(&self) {
        // Busy entries are deferred to the end of the pass rather than
        // requeued on the spot: the class queue is shared across virtual
        // models, so a request whose members are all busy must not
        // head-of-line-block requests targeting free deployments.
        let mut deferred: Vec<(PendingEntry, String, bool)> = Vec::new();
        for _ in 0..128 {
            let mut c = self.inner.rt.conn.clone();
            let mut keys: Vec<String> = (0..self.inner.cfg.priority_classes.max(1))
                .map(|p| self.inner.rt.k(&format!("queue:{p}")))
                .collect();
            // Trailing bookkeeping keys: the script decrements queue:count and
            // removes the watchdog member atomically with the pop.
            keys.push(self.inner.rt.k("queue:count"));
            keys.push(self.inner.rt.k("req:watchdog"));
            let popped: Result<Option<String>, _> = redis::Script::new(crate::lua::QUEUE_POP)
                .key(keys)
                .invoke_async(&mut c)
                .await;
            match &popped {
                Err(e) => tracing::warn!(error=%e, "queue_pop failed"),
                Ok(v) => tracing::debug!(popped=?v, "queue_pop result"),
            }
            let Ok(Some(rid)) = popped else { break };
            let entry = self.inner.pending.lock().await.remove(&rid);
            let Some(e) = entry else {
                // Orphaned queue entry — decide whose it is. With a shared
                // Redis namespace a peer server may own the in-process waiter:
                // dropping the tracking hash would hang the caller on the
                // other instance until deadline. Requeue it back (FIFO score
                // preserved) so its owner pops it next pass. Only entries
                // with no owner stamp — or our own — are true orphans.
                let h: std::collections::HashMap<String, String> = redis::cmd("HGETALL")
                    .arg(self.inner.rt.k(&format!("req:{rid}")))
                    .query_async(&mut c)
                    .await
                    .unwrap_or_default();
                let foreign = h
                    .get("owner")
                    .is_some_and(|o| !o.is_empty() && o != &self.inner.instance_id);
                if foreign {
                    let prio: u32 = h.get("prio").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let ets: i64 = h
                        .get("enqueued_at")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(crate::client_registry::now_ms);
                    let dl: i64 = h
                        .get("deadline")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(i64::MAX);
                    let proj = h.get("project_id").cloned().unwrap_or_default();
                    let model = h.get("vm_id").cloned().unwrap_or_default();
                    let owner = h.get("owner").cloned().unwrap_or_default();
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
                        .arg(&owner)
                        .invoke_async(&mut c)
                        .await
                        .unwrap_or_else(|_| "ERR".into());
                    if res == "OK" {
                        continue;
                    }
                    // queue full or script error — fall through to drop.
                }
                let _: () = redis::cmd("DEL")
                    .arg(self.inner.rt.k(&format!("req:{rid}")))
                    .query_async(&mut c)
                    .await
                    .unwrap_or(());
                continue;
            };
            // Dead waiter: the HTTP request already timed out or the caller
            // disconnected — dispatching would block the queue head and burn
            // fleet capacity for a response nobody reads.
            if e.done_tx.is_closed() {
                let mut c = self.inner.rt.conn.clone();
                let _: () = redis::cmd("DEL")
                    .arg(self.inner.rt.k(&format!("req:{rid}")))
                    .query_async(&mut c)
                    .await
                    .unwrap_or(());
                continue;
            }
            let deadline = std::time::Instant::now()
                + Duration::from_millis(self.inner.cfg.assign_ack_timeout_ms.max(5000) + 30_000);
            let outcome = self.try_dispatch(&e.req, deadline).await;
            match outcome {
                Ok(outcome) => {
                    // Dispatched: the lease now tracks inflight state — the
                    // queued tracking hash is write-only metadata and would
                    // otherwise linger until its TTL.
                    let mut c = self.inner.rt.conn.clone();
                    let _: () = redis::cmd("DEL")
                        .arg(self.inner.rt.k(&format!("req:{rid}")))
                        .query_async(&mut c)
                        .await
                        .unwrap_or(());
                    let _ = e.done_tx.send(Ok(outcome));
                }
                Err(SchedErr::NoCapacity) => {
                    // Members busy but reachable: defer — the entry keeps its
                    // FIFO score and is retried next pass, but does not block
                    // requests behind it that target different members.
                    deferred.push((e, rid, false));
                    continue;
                }
                Err(SchedErr::AllUnavailable) => {
                    // Grace window: freshly enqueued requests (<5s) are
                    // likely seeing a reconnect/report gap — requeue uncounted
                    // so a wake (capacity report) retries them instead of
                    // burning all attempts within one burst. Older requests
                    // count attempts normally and die bounded.
                    let fresh = crate::client_registry::now_ms() - e.req.enqueue_ts < 5_000;
                    deferred.push((e, rid, !fresh));
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
        for (e, rid, count) in deferred {
            self.requeue(e, &rid, count).await;
        }
        self.sync_queue_depth().await;
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
            .arg(&self.inner.instance_id)
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

    async fn try_dispatch(
        &self,
        req: &PendingRequest,
        deadline: std::time::Instant,
    ) -> Result<RequestOutcome, SchedErr> {
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
        // Structural check before routing: when the footprint exceeds every
        // member's KV budget this request can never dispatch — fail fast
        // instead of blocking the queue head until its deadline. Members
        // without a budget (slot-only mode) always "fit".
        let fits_anywhere = candidates.iter().any(|m| {
            match m.admission_mode.as_str() {
                // Concurrency-only: the token gate is off entirely.
                "slots" => true,
                // Token gate REQUIRED: a member without a budget cannot
                // honestly admit anything (budget may still arrive via a
                // capacity report or admin override — count it as unfit
                // here so it neither blocks nor falsely "fits").
                "tokens" => m
                    .kv_budget
                    .is_some_and(|b| b > 0 && req.footprint_tokens <= b as u64),
                // auto: token gate when a positive budget exists, slot-only
                // otherwise.
                _ => m
                    .kv_budget
                    .is_none_or(|b| b <= 0 || req.footprint_tokens <= b as u64),
            }
        });
        if !fits_anywhere {
            return Err(SchedErr::ContextTooLarge);
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
        // A member whose client still reports in — `available` or `draining`,
        // regardless of health — is by construction transient: it may be
        // pressure-gated, mid-rewarm, or recovering behind a generation
        // breaker, and rejoins on its own within seconds. Requests queued
        // behind it must wait (bounded by queue_timeout), not burn attempts
        // and die just before an ~8s recovery completes. Only members that
        // are unreachable outright — `offline`/`unavailable`, i.e. session
        // dead or deployment removed — make the shortage structural.
        let any_busy = ranked.is_empty()
            && runtime
                .values()
                .any(|(_, st, _)| st == "available" || st == "draining");
        let mut saw_transient = false;
        for member in ranked {
            // The dispatch pass is time-budgeted, but a deadline is only
            // checked BETWEEN candidates — an in-flight lease_and_assign runs
            // its own bounded awaits to completion. Dropping it mid-await
            // would leave an acquired lease nobody owns.
            if std::time::Instant::now() >= deadline {
                tracing::debug!(req = %req.request_id, "dispatch budget exhausted mid-pass");
                return Err(SchedErr::NoCapacity);
            }
            match self.lease_and_assign(req, &member).await {
                Ok(outcome) => {
                    trace["deployment_id"] = json!(member.deployment_id);
                    trace["client_id"] = json!(member.client_id);
                    return Ok(RequestOutcome {
                        routing_trace: trace,
                        queue_wait_ms: (crate::client_registry::now_ms() - req.enqueue_ts).max(0),
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
        Err(if saw_transient || any_busy {
            SchedErr::NoCapacity
        } else {
            SchedErr::AllUnavailable
        })
    }

    async fn runtime_view(
        &self,
        candidates: &[crate::model_registry::Member],
    ) -> HashMap<String, (u32, String, String)> {
        let mut out = HashMap::new();
        for m in candidates {
            if let Ok(Some(rt)) = self.inner.registry.dep_runtime(&m.deployment_id).await {
                out.insert(
                    m.deployment_id.clone(),
                    (rt.tier2_free, rt.state, rt.health),
                );
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
        // Resolve the effective budget by admission mode: "slots" disables
        // the token gate (budget=0 → slot-only path in the script), "tokens"
        // requires a positive budget — without one the member defers until
        // a report/override lands.
        let budget = match member.admission_mode.as_str() {
            "slots" => 0,
            _ => member.kv_budget.unwrap_or(0),
        };
        if member.admission_mode == "tokens" && budget <= 0 {
            return Err(SchedErr::NoCapacity);
        }
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
            .arg(req.footprint_tokens as i64)
            .arg(budget)
            .invoke_async(&mut c)
            .await
            .map_err(|e| SchedErr::Backend(e.to_string()))?;
        let fencing = match &res {
            redis::Value::Array(v) => match v.first() {
                Some(redis::Value::Int(i)) => *i as u64,
                _ => 0,
            },
            // KV_FULL shares the transient-defer path with slot exhaustion —
            // the budget frees as running requests finish. The distinct
            // return code exists so debug logs can tell them apart.
            redis::Value::SimpleString(s) if s == "KV_FULL" => {
                tracing::debug!(req = %req.request_id, dep = %dep_id, fp = req.footprint_tokens, "member kv budget full");
                return Err(SchedErr::NoCapacity);
            }
            redis::Value::BulkString(b) if b == b"KV_FULL" => {
                tracing::debug!(req = %req.request_id, dep = %dep_id, fp = req.footprint_tokens, "member kv budget full");
                return Err(SchedErr::NoCapacity);
            }
            redis::Value::SimpleString(s) if s == "NO_CAPACITY" => {
                return Err(SchedErr::NoCapacity)
            }
            redis::Value::BulkString(b) if b == b"NO_CAPACITY" => return Err(SchedErr::NoCapacity),
            redis::Value::SimpleString(s) if s == "UNAVAILABLE" => {
                return Err(SchedErr::ClientOffline)
            }
            redis::Value::BulkString(b) if b == b"UNAVAILABLE" => {
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
            req.footprint_tokens,
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

        // Confirm lease → then await client data stream. A vanished lease
        // (swept between acquire and confirm) must abort the assign —
        // proceeding would stream a request that holds no slot.
        let conf: String = redis::Script::new(crate::lua::LEASE_CONFIRM)
            .key(self.inner.rt.k(&format!("lease:{lease_id}")))
            .key(self.inner.rt.k(&format!("dep:{dep_id}")))
            .arg(fencing)
            .invoke_async(&mut c)
            .await
            .unwrap_or_default();
        if conf != "OK" {
            self.cleanup_assign(&rid, &lease_id, fencing, &dep_id).await;
            return Err(SchedErr::ClientOffline);
        }

        // activate lease state
        let act: String = redis::Script::new(crate::lua::LEASE_ACTIVATE)
            .key(self.inner.rt.k(&format!("lease:{lease_id}")))
            .arg(fencing)
            .arg(crate::client_registry::now_ms())
            .invoke_async(&mut c)
            .await
            .unwrap_or_default();
        if act != "OK" {
            self.cleanup_assign(&rid, &lease_id, fencing, &dep_id).await;
            return Err(SchedErr::ClientOffline);
        }

        let (mut inbound, outbound) =
            match tokio::time::timeout(Duration::from_secs(10), data_rx).await {
                Ok(Ok(pair)) => pair,
                _ => {
                    self.cleanup_assign(&rid, &lease_id, fencing, &dep_id).await;
                    return Err(SchedErr::ClientOffline);
                }
            };

        self.inner.active.lock().await.insert(
            rid.clone(),
            (client_id.clone(), fencing, lease_id.clone(), dep_id.clone()),
        );

        // Bridge task: send request payload via outbound frames, read
        // response_chunk frames from inbound into resp channel.
        let (resp_tx, resp_rx) = mpsc::channel::<Result<bytes::Bytes, SchedErr>>(32);
        let upstream_status = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let cancel = CancellationToken::new();
        {
            let body = req.body.clone();
            let up_status = upstream_status.clone();
            let cancel2 = cancel.clone();
            let rt = self.inner.rt.clone();
            let inner2 = self.inner.clone();
            let sessions2 = self.inner.sessions.clone();
            let client2 = client_id.clone();
            let lease2 = lease_id.clone();
            let dep2 = dep_id.clone();
            let rid_for_release = rid.clone();
            tokio::spawn(async move {
                use futures_util::StreamExt;
                // Stream liveness: the lease's `activated_at` is what the
                // watchdog sweep compares against `stream_idle_timeout_secs`.
                // Refreshing it on data turns that hard deadline into a pure
                // stall timeout — a generating request is never killed.
                let mut last_liveness = std::time::Instant::now();
                let mut end_status = "done";
                let mut end_error = String::new();
                'work: {
                    // 1) deliver payload to client (chunked). A caller
                    // disconnect during a slow send must abort here too —
                    // the receive loop's select arms only run afterwards.
                    for chunk in body.chunks(256 * 1024) {
                        let send = outbound.send(frames::request_json(chunk.to_vec()));
                        tokio::pin!(send);
                        let sent = tokio::select! {
                            _ = cancel2.cancelled() => { end_status = "cancelled"; break 'work },
                            _ = resp_tx.closed() => { end_status = "cancelled"; break 'work },
                            r = &mut send => r,
                        };
                        if sent.is_err() {
                            let _ = resp_tx
                                .send(Err(SchedErr::Backend("data stream closed".into())))
                                .await;
                            end_status = "failed";
                            end_error = "data_stream_closed".into();
                            break 'work;
                        }
                    }
                    // 2) forward client response chunks until End/Error/cancel
                    tracing::debug!(req = %rid_for_release, "bridge: payload sent, reading inbound");
                    loop {
                        let next = tokio::select! {
                            _ = cancel2.cancelled() => { end_status = "cancelled"; break },
                            // Caller dropped the response receiver — abort
                            // immediately instead of waiting for the next
                            // upstream chunk to notice the dead channel.
                            // resp_tx is the bridge's own sender (no clone),
                            // so channel lifetime is unchanged.
                            _ = resp_tx.closed() => {
                                let _ = sessions2
                                    .send(
                                        &client2,
                                        server_msgs::cancel(
                                            &rid_for_release,
                                            fencing,
                                            "caller_disconnected",
                                        ),
                                    )
                                    .await;
                                end_status = "cancelled";
                                break;
                            }
                            v = inbound.next() => v,
                        };
                        tracing::debug!(req = %rid_for_release, frame = ?next.as_ref().map(|r| r.as_ref().map(|f| std::mem::discriminant(&f.frame))), "bridge inbound");
                        match next {
                            Some(Ok(loom_protocol::pb::DataFrame {
                                frame: Some(loom_protocol::pb::data_frame::Frame::ResponseChunk(b)),
                            })) => {
                                if last_liveness.elapsed() > Duration::from_secs(10) {
                                    last_liveness = std::time::Instant::now();
                                    let mut conn = rt.conn.clone();
                                    // EXISTS guard: a plain HSET on a vanished lease
                                    // would recreate a partial hash (activated_at
                                    // only) that no sweep can attribute to a state.
                                    let _: () = redis::Script::new(
                                        "if redis.call('EXISTS', KEYS[1]) == 1 then \
                                         redis.call('HSET', KEYS[1], 'activated_at', ARGV[1]) end",
                                    )
                                    .key(rt.k(&format!("lease:{lease2}")))
                                    .arg(crate::client_registry::now_ms())
                                    .invoke_async(&mut conn)
                                    .await
                                    .unwrap_or(());
                                }
                                tracing::debug!(req = %rid_for_release, n = b.len(), "response_chunk");
                                if resp_tx.send(Ok(b.into())).await.is_err() {
                                    // Caller gone: tell the client to abort backend
                                    // generation — merely stop reading would leave
                                    // the model burning a tier-2 slot until natural
                                    // completion.
                                    let _ = sessions2
                                        .send(
                                            &client2,
                                            server_msgs::cancel(
                                                &rid_for_release,
                                                fencing,
                                                "caller_disconnected",
                                            ),
                                        )
                                        .await;
                                    end_status = "cancelled";
                                    break;
                                }
                            }
                            Some(Ok(loom_protocol::pb::DataFrame {
                                frame: Some(loom_protocol::pb::data_frame::Frame::Error(e)),
                            })) => {
                                let _ = resp_tx
                                    .send(Err(SchedErr::Backend(e.message.clone())))
                                    .await;
                                end_status = "failed";
                                end_error = "backend".into();
                                break;
                            }
                            Some(Ok(loom_protocol::pb::DataFrame {
                                frame: Some(loom_protocol::pb::data_frame::Frame::End(e)),
                            })) => {
                                up_status
                                    .store(e.upstream_status, std::sync::atomic::Ordering::Relaxed);
                                if e.cancelled {
                                    end_status = "cancelled";
                                }
                                tracing::debug!(req = %rid_for_release, "end frame");
                                break;
                            }
                            Some(Ok(_)) => continue,
                            Some(Err(e)) => {
                                let _ = resp_tx.send(Err(SchedErr::Backend(e.to_string()))).await;
                                end_status = "failed";
                                end_error = "data_stream".into();
                                break;
                            }
                            None => {
                                end_status = "aborted";
                                end_error = "data_stream_closed".into();
                                break;
                            }
                        }
                    }
                }
                // Close the caller-visible response stream immediately: all
                // response bytes were already sent inside 'work, and holding
                // resp_tx through the RequestEnd grace below would keep the
                // HTTP body open ~15s past the last token — clients would
                // keep showing "running" after the answer completed.
                drop(resp_tx);
                // The data phase is over — the entry must leave `active`
                // BEFORE the lease is released: the watchdog reconcile
                // treats `active` + missing lease as a crash and would
                // overwrite a completed request's history with
                // `failed/lease_lost` whenever its tick lands in the
                // release→RequestEnd gap.
                inner2.active.lock().await.remove(&rid_for_release);
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
                // The freed slot is dispatchable capacity — wake the loop now
                // instead of letting queued work wait out the 2s tick.
                inner2.wake.notify_one();
                // Safety net for a missing RequestEnd: the client normally
                // reports usage/end on the control stream within ms, but if
                // it died mid-teardown (or is non-conforming) `ends`/`active`
                // and the gateway finalizer would hang forever — leaking the
                // inflight counter and the usage record. Give the real
                // RequestEnd a short grace window, then resolve synthetically.
                tokio::time::sleep(Duration::from_secs(15)).await;
                if let Some(tx) = inner2.ends.lock().await.remove(&rid_for_release) {
                    let _ = tx.send(EndMeta {
                        status: end_status.into(),
                        error_code: end_error,
                        ..Default::default()
                    });
                    inner2.active.lock().await.remove(&rid_for_release);
                    inner2.wake.notify_one();
                }
            });
        }
        Ok(RequestOutcome {
            response_rx: resp_rx,
            upstream_status,
            end_rx,
            routing_trace: json!({}),
            deployment_id: dep_id,
            client_id,
            lease_id,
            cancel,
            queue_wait_ms: 0,
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

/// Clamp a derived priority class to the valid queue range `0..=classes-1`.
/// `classes` is normalized to ≥1 so the result always names a queue the pop
/// path actually reads — an out-of-range class would be an invisible tomb.
fn clamp_class(prio: u32, classes: u32) -> u32 {
    prio.min(classes.max(1) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_class_bounds_to_valid_queues() {
        assert_eq!(clamp_class(0, 4), 0);
        assert_eq!(clamp_class(3, 4), 3);
        assert_eq!(clamp_class(99, 4), 3, "out-of-range clamps to last class");
        assert_eq!(clamp_class(5, 1), 0, "single-class config clamps to 0");
        assert_eq!(clamp_class(2, 0), 0, "zero-class config normalizes to 1");
        assert_eq!(clamp_class(0, 0), 0);
    }
}
