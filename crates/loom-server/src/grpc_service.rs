//! Agent gRPC service: control stream, per-request data streams, GetConfig.

use crate::client_registry::{now_ms, ClientRegistry};
use crate::scheduler::{EndMeta, Scheduler};
use crate::session::SessionManager;
use futures_util::{Stream, StreamExt};
use loom_protocol::pb::agent_server::{Agent, AgentServer};
use loom_protocol::pb::*;
use loom_protocol::{limits, server_msgs};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::warn;

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[derive(Clone)]
pub struct AgentSvc {
    pub sessions: Arc<SessionManager>,
    pub registry: ClientRegistry,
    pub scheduler: Scheduler,
    /// Internal CA for mTLS client-cert issuance (None when PKI disabled).
    pub client_ca: Option<crate::tls_ca::ClientCa>,
}

pub fn server(svc: AgentSvc) -> AgentServer<AgentSvc> {
    AgentServer::new(svc)
}

#[tonic::async_trait]
impl Agent for AgentSvc {
    type ControlStream = BoxStream<ServerEnvelope>;

    async fn control(
        &self,
        request: Request<Streaming<ClientEnvelope>>,
    ) -> Result<Response<Self::ControlStream>, Status> {
        let mut inbound = request.into_inner();
        let (out_tx, out_rx) = mpsc::channel::<ServerEnvelope>(64);
        let this = self.clone();

        tokio::spawn(async move {
            // --- Hello / auth ---
            let hello = match inbound.next().await {
                Some(Ok(ClientEnvelope {
                    msg: Some(client_envelope::Msg::Hello(h)),
                })) => h,
                _ => return,
            };
            if !loom_protocol::version::compatible(
                hello.protocol_version_min,
                hello.protocol_version,
            ) || hello.protocol_version > limits::PROTOCOL_VERSION
            {
                let _ = out_tx
                    .send(server_msgs::hello_ack(
                        false,
                        "protocol_version_unsupported",
                        "",
                        "",
                        "",
                        0,
                        10,
                    ))
                    .await;
                return;
            }
            let auth = this
                .registry
                .authenticate(
                    &hello.client_id,
                    &hello.credential,
                    &hello.client_name,
                    &hello.client_version,
                    &hello.platform,
                )
                .await
                .unwrap_or_default();
            let Some((client_id, issued_cred)) = auth else {
                let _ = out_tx
                    .send(server_msgs::hello_ack(false, "rejected", "", "", "", 0, 10))
                    .await;
                return;
            };
            if hello.client_version != env!("CARGO_PKG_VERSION") {
                warn!(
                    client = %client_id,
                    client_version = %hello.client_version,
                    server_version = env!("CARGO_PKG_VERSION"),
                    "client version skew — semantics may differ"
                );
            }
            // mTLS issuance: a client that presented a CSR gets a signed
            // cert + CA chain back. The transport layer (client_ca_root on
            // the strict listener) enforces possession of the private key.
            let (cert_pem, ca_pem, cert_exp) =
                match (hello.client_csr_pem.is_empty(), &this.client_ca) {
                    (false, Some(ca)) => match ca.sign_csr(&hello.client_csr_pem, &client_id) {
                        Ok((p, exp)) => (p, ca.ca_pem().to_string(), exp),
                        Err(e) => {
                            warn!(client = %client_id, error = %e, "client CSR signing failed");
                            (String::new(), String::new(), 0)
                        }
                    },
                    _ => (String::new(), String::new(), 0),
                };
            let session_id = uuid::Uuid::new_v4().to_string();
            if out_tx
                .send(server_msgs::hello_ack_pki(
                    true,
                    "",
                    &session_id,
                    &client_id,
                    &issued_cred,
                    0,
                    10,
                    &cert_pem,
                    &ca_pem,
                    cert_exp,
                ))
                .await
                .is_err()
            {
                return;
            }
            let _ = this
                .registry
                .set_online(&client_id, &session_id, &hello.client_version)
                .await;
            this.sessions.register(&client_id, out_tx.clone()).await;
            // Current notes/briefings for this client (broadcast + scoped).
            crate::notes::push_to(&this.sessions, &this.registry.db, &client_id).await;
            if let Some(inv) = &hello.inventory {
                let _ = this.registry.apply_inventory(&client_id, inv).await;
            }
            this.scheduler.wake();
            tracing::info!(client = %client_id, session = %session_id, "client connected");

            // --- control loop ---
            while let Some(item) = inbound.next().await {
                let Ok(env) = item else { break };
                let Some(msg) = env.msg else { continue };
                match msg {
                    client_envelope::Msg::Heartbeat(h) => {
                        let _ = this.registry.heartbeat(&client_id, &h.session_id).await;
                    }
                    client_envelope::Msg::CapacityReport(r) => {
                        let _ = this
                            .registry
                            .apply_capacity(&client_id, &r.capacities, r.host.as_ref())
                            .await;
                        this.scheduler.wake();
                    }
                    client_envelope::Msg::InventoryReport(r) => {
                        let _ = this.registry.apply_inventory(&client_id, &r).await;
                    }
                    client_envelope::Msg::AssignAck(a) => {
                        let (ok, reason) = match a.result {
                            Some(assign_ack::Result::Accepted(_)) => (true, String::new()),
                            Some(assign_ack::Result::Reject(r)) => (false, r.reason),
                            None => (false, "no_result".into()),
                        };
                        this.scheduler
                            .handle_assign_ack(&a.request_id, ok, &reason)
                            .await;
                    }
                    client_envelope::Msg::RequestEnd(e) => {
                        this.scheduler
                            .complete_end(
                                &e.request_id,
                                EndMeta {
                                    status: e.status,
                                    error_code: e.error_code,
                                    prompt_tokens: e.prompt_tokens,
                                    completion_tokens: e.completion_tokens,
                                    ttft_ms: e.ttft_ms,
                                    duration_ms: e.duration_ms,
                                },
                            )
                            .await;
                    }
                    client_envelope::Msg::ErrorReport(e) => {
                        tracing::warn!(code = %e.code, req = %e.request_id, "client error report");
                    }
                    client_envelope::Msg::ConfigAck(a) => {
                        tracing::debug!(
                            version = a.config_version,
                            applied = a.applied,
                            "config ack"
                        );
                        if a.applied {
                            // Consume one-shot directives (update_now) so they
                            // don't re-fire on the next GetConfig pull.
                            let _ = sqlx::query(
                                "UPDATE clients SET desired_config = desired_config #- '{client,updater,update_now}' WHERE id = $1::uuid",
                            )
                            .bind(&client_id)
                            .execute(&this.registry.db)
                            .await;
                        }
                    }
                    client_envelope::Msg::Pong(_) => {}
                    client_envelope::Msg::Hello(_) => {}
                }
            }

            // disconnect: release leases, mark offline, fail/requeue requests
            this.sessions.unregister(&client_id).await;
            match this.registry.client_offline(&client_id).await {
                Ok((requeue, fail)) => {
                    // reserved leases → nudge the awaiting dispatcher to retry
                    // the next candidate immediately (reject ack + drop the
                    // data-stream expectation so its oneshot resolves).
                    for rid in &requeue {
                        this.scheduler
                            .handle_assign_ack(rid, false, "client_offline")
                            .await;
                        this.scheduler.drop_expectation(rid).await;
                    }
                    // active leases → controlled failure for the caller.
                    for rid in &fail {
                        this.scheduler
                            .complete_end(
                                rid,
                                EndMeta {
                                    status: "failed".into(),
                                    error_code: "client_offline".into(),
                                    ..Default::default()
                                },
                            )
                            .await;
                    }
                }
                Err(e) => tracing::error!(error = %e, "client_offline failed"),
            }
            this.scheduler.wake();
            tracing::info!(client = %client_id, "client disconnected");
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(out_rx).map(Ok))))
    }

    type RequestDataStream = BoxStream<DataFrame>;

    async fn request_data(
        &self,
        request: Request<Streaming<DataFrame>>,
    ) -> Result<Response<Self::RequestDataStream>, Status> {
        let mut inbound = request.into_inner();
        let (out_tx, out_rx) = mpsc::channel::<DataFrame>(64);
        let sched = self.scheduler.clone();

        // Read head frame for correlation.
        let head = match inbound.next().await {
            Some(Ok(DataFrame {
                frame: Some(data_frame::Frame::Head(h)),
            })) => h,
            _ => return Err(Status::invalid_argument("missing head frame")),
        };
        // Fencing check: head must present a live lease for this request —
        // guards against stale data streams after reconnect/retry.
        let mut conn = self.registry.rt.conn.clone();
        let lf: Option<String> = redis::cmd("HGET")
            .arg(self.registry.rt.k(&format!("lease:{}", head.lease_id)))
            .arg("fencing")
            .query_async(&mut conn)
            .await
            .unwrap_or(None);
        let lrq: Option<String> = redis::cmd("HGET")
            .arg(self.registry.rt.k(&format!("lease:{}", head.lease_id)))
            .arg("request_id")
            .query_async(&mut conn)
            .await
            .unwrap_or(None);
        let stale = match (lrq, lf) {
            (Some(rq), Some(f)) => rq != head.request_id || f != head.fencing_token.to_string(),
            _ => true,
        };
        if stale {
            let (tx, rx) = mpsc::channel::<DataFrame>(1);
            let _ = tx
                .send(loom_protocol::frames::error(
                    "stale_fencing",
                    "lease fencing token mismatch",
                    false,
                ))
                .await;
            drop(tx);
            return Ok(Response::new(Box::pin(ReceiverStream::new(rx).map(Ok))));
        }
        let delivered = sched.deliver_data(&head.request_id, inbound, out_tx).await;
        if !delivered {
            let (tx, rx) = mpsc::channel::<DataFrame>(1);
            let _ = tx
                .send(loom_protocol::frames::error(
                    "unknown_request",
                    "no pending assignment",
                    false,
                ))
                .await;
            drop(tx);
            return Ok(Response::new(Box::pin(ReceiverStream::new(rx).map(Ok))));
        }
        let _ = now_ms();
        Ok(Response::new(Box::pin(ReceiverStream::new(out_rx).map(Ok))))
    }

    async fn get_config(
        &self,
        request: Request<ConfigRequest>,
    ) -> Result<Response<ConfigSnapshot>, Status> {
        let req = request.into_inner();
        // authorize: device credential check
        if req.credential.is_empty() {
            return Err(Status::unauthenticated("bad credential"));
        }
        let cred_hash = loom_core::ids::sha256_hex(&req.credential);
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT status FROM clients WHERE id = $1::uuid AND credential_hash = $2",
        )
        .bind(&req.client_id)
        .bind(&cred_hash)
        .fetch_optional(&self.registry.db)
        .await
        .map_err(|_| Status::internal("db"))?;
        match row {
            Some((s,)) if s == "active" => {
                #[derive(sqlx::FromRow)]
                struct Cfg {
                    config_version: i64,
                    desired_config: serde_json::Value,
                }
                let c: Option<Cfg> = sqlx::query_as(
                    "SELECT config_version, desired_config FROM clients WHERE id = $1::uuid",
                )
                .bind(&req.client_id)
                .fetch_optional(&self.registry.db)
                .await
                .unwrap_or(None);
                let (version, desired) = c
                    .map(|c| {
                        (
                            c.config_version.max(0) as u64,
                            serde_json::to_vec(&c.desired_config)
                                .unwrap_or_else(|_| b"{}".to_vec()),
                        )
                    })
                    .unwrap_or((0, b"{}".to_vec()));
                Ok(Response::new(ConfigSnapshot {
                    config_version: version,
                    desired_config_json: desired,
                }))
            }
            _ => Err(Status::unauthenticated("bad credential")),
        }
    }
}
