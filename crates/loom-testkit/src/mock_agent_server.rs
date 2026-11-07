//! Mock Agent server implementing the `Agent` gRPC service — used to test the
//! client before the real server exists (and for failure-injection tests).

use futures_util::{Stream, StreamExt};
use loom_protocol::pb::agent_server::{Agent, AgentServer};
use loom_protocol::pb::*;
use loom_protocol::{frames, server_msgs};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[derive(Debug)]
pub struct AssignedRequest {
    pub request_id: String,
    pub lease_id: String,
    /// Stream of response chunks received from the client.
    pub response_rx: mpsc::Receiver<bytes::Bytes>,
    /// Completes when request ended (End frame / error / disconnect).
    pub done_rx: oneshot::Receiver<String>,
}

#[derive(Clone)]
pub struct MockAgentServer {
    inner: Arc<Inner>,
}

struct Inner {
    /// Connected clients' control senders: client_id -> sender.
    sessions: Mutex<HashMap<String, mpsc::Sender<ServerEnvelope>>>,
    /// Pending data-stream correlators: request_id -> pending state.
    pending: Mutex<HashMap<String, PendingData>>,
    /// Credentials accepted by the mock server (empty = accept any).
    accept_credentials: Mutex<Vec<String>>,
    /// Whether to accept hellos.
    auto_accept: AtomicBool,
    /// Reject assigns (simulate client-level rejections not needed server side,
    /// but let tests observe ack capture).
    hb_secs: u32,
    /// Captured messages for assertions.
    pub captured: Mutex<Vec<String>>,
}

struct PendingData {
    payload: Vec<u8>,
    resp_tx: mpsc::Sender<bytes::Bytes>,
    done_tx: Option<oneshot::Sender<String>>,
    ready_tx: Option<oneshot::Sender<()>>,
    fencing: u64,
}

impl Default for MockAgentServer {
    fn default() -> Self {
        Self::new()
    }
}

impl MockAgentServer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                accept_credentials: Mutex::new(Vec::new()),
                auto_accept: AtomicBool::new(true),
                hb_secs: 5,
                captured: Mutex::new(Vec::new()),
            }),
        }
    }

    pub async fn accept_credential(&self, cred: &str) {
        self.inner
            .accept_credentials
            .lock()
            .await
            .push(cred.to_string());
    }

    pub fn set_auto_accept(&self, v: bool) {
        self.inner.auto_accept.store(v, Ordering::SeqCst);
    }

    /// Send an AssignRequest over the control stream of the first connected
    /// client. Resolves once the client opened its RequestData stream and the
    /// payload has been forwarded — the AssignedRequest is then pushed to the
    /// `assigned` channel returned from `new()` (not used here; we return it).
    pub async fn assign(
        &self,
        deployment_id: &str,
        api_type: &str,
        payload: Vec<u8>,
        virtual_model: &str,
    ) -> Result<oneshot::Receiver<AssignedRequest>, Status> {
        let request_id = Uuid::new_v4().to_string();
        let lease_id = Uuid::new_v4().to_string();
        let fencing: u64 = 1;

        let (resp_tx, resp_rx) = mpsc::channel(64);
        let (done_tx, done_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (assigned_tx, assigned_rx) = oneshot::channel();

        let payload_len_hint = payload.len() as u32;
        self.inner.pending.lock().await.insert(
            request_id.clone(),
            PendingData {
                payload,
                resp_tx,
                done_tx: Some(done_tx),
                ready_tx: Some(ready_tx),
                fencing,
            },
        );

        let sessions = self.inner.sessions.lock().await;
        let Some(sender) = sessions.values().next() else {
            return Err(Status::unavailable("no client connected"));
        };
        sender
            .send(server_msgs::assign(
                &request_id,
                &lease_id,
                fencing,
                server_msgs::now_ms() + 60_000,
                api_type,
                RequiredCaps::default(),
                payload_len_hint,
                virtual_model,
                deployment_id,
                "application/json",
            ))
            .await
            .map_err(|_| Status::unavailable("control stream closed"))?;
        drop(sessions);

        tokio::spawn(async move {
            if let Ok(Ok(())) =
                tokio::time::timeout(std::time::Duration::from_secs(10), ready_rx).await
            {
                let _ = assigned_tx.send(AssignedRequest {
                    request_id,
                    lease_id,
                    response_rx: resp_rx,
                    done_rx,
                });
            }
        });
        Ok(assigned_rx)
    }

    /// Push a ConfigPush to the first connected client.
    pub async fn push_config(&self, version: u64, payload: Vec<u8>) {
        let sessions = self.inner.sessions.lock().await;
        if let Some(sender) = sessions.values().next() {
            let _ = sender
                .send(server_msgs::config_push(version, payload, true))
                .await;
        }
    }

    /// Send a cancel for a request id to the first session.
    pub async fn cancel(&self, request_id: &str, fencing: u64) {
        let sessions = self.inner.sessions.lock().await;
        if let Some(sender) = sessions.values().next() {
            let _ = sender
                .send(server_msgs::cancel(request_id, fencing, "test-cancel"))
                .await;
        }
    }

    pub async fn captured(&self) -> Vec<String> {
        self.inner.captured.lock().await.clone()
    }

    /// Spawn on an ephemeral port (plaintext). Returns `http://addr`.
    pub async fn spawn(self) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let svc = AgentServer::new(self);
        let handle = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(svc)
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .ok();
        });
        (format!("http://{addr}"), handle)
    }
}

#[tonic::async_trait]
impl Agent for MockAgentServer {
    type ControlStream = BoxStream<ServerEnvelope>;

    async fn control(
        &self,
        request: Request<Streaming<ClientEnvelope>>,
    ) -> Result<Response<Self::ControlStream>, Status> {
        let mut inbound = request.into_inner();
        let (out_tx, out_rx) = mpsc::channel::<ServerEnvelope>(64);
        let inner = self.inner.clone();

        tokio::spawn(async move {
            let first = match inbound.next().await {
                Some(Ok(env)) => env,
                _ => return,
            };
            let Some(client_envelope::Msg::Hello(hello)) = first.msg else {
                return;
            };

            if !crate::harness::proto_compatible(hello.protocol_version_min, hello.protocol_version)
            {
                let _ = out_tx
                    .send(server_msgs::hello_ack(
                        false,
                        "protocol_version_unsupported",
                        "",
                        "",
                        "",
                        0,
                        inner.hb_secs,
                    ))
                    .await;
                return;
            }

            let creds = inner.accept_credentials.lock().await.clone();
            let cred_ok = creds.is_empty() || creds.contains(&hello.credential);
            let accept = inner.auto_accept.load(Ordering::SeqCst) && cred_ok;

            let session_id = Uuid::new_v4().to_string();
            let client_id = if hello.client_id.is_empty() {
                Uuid::new_v4().to_string()
            } else {
                hello.client_id.clone()
            };
            let issued_cred = if hello.credential.starts_with("enroll_") {
                format!("dev_{}", Uuid::new_v4().simple())
            } else {
                String::new()
            };
            if !accept {
                let _ = out_tx
                    .send(server_msgs::hello_ack(
                        false,
                        "rejected",
                        "",
                        "",
                        "",
                        0,
                        inner.hb_secs,
                    ))
                    .await;
                return;
            }
            if out_tx
                .send(server_msgs::hello_ack(
                    true,
                    "",
                    &session_id,
                    &client_id,
                    &issued_cred,
                    0,
                    inner.hb_secs,
                ))
                .await
                .is_err()
            {
                return;
            }
            inner
                .sessions
                .lock()
                .await
                .insert(client_id.clone(), out_tx.clone());
            inner
                .captured
                .lock()
                .await
                .push(format!("hello:{client_id}"));

            while let Some(msg) = inbound.next().await {
                let Ok(env) = msg else { break };
                let Some(m) = env.msg else { continue };
                let tag = match m {
                    client_envelope::Msg::Heartbeat(_) => "heartbeat".to_string(),
                    client_envelope::Msg::CapacityReport(r) => {
                        format!("capacity:{}", r.capacities.len())
                    }
                    client_envelope::Msg::InventoryReport(r) => {
                        format!("inventory:{}", r.deployments.len())
                    }
                    client_envelope::Msg::ConfigAck(a) => {
                        format!("config_ack:{}", a.config_version)
                    }
                    client_envelope::Msg::AssignAck(ack) => {
                        let ok = matches!(ack.result, Some(assign_ack::Result::Accepted(true)));
                        format!("assign_ack:{}:{}", ack.request_id, ok)
                    }
                    client_envelope::Msg::RequestEnd(e) => {
                        format!("end:{}:{}", e.request_id, e.status)
                    }
                    client_envelope::Msg::ErrorReport(e) => {
                        format!("err:{}:{}", e.code, e.request_id)
                    }
                    client_envelope::Msg::Pong(_) => continue,
                    client_envelope::Msg::Hello(_) => "hello_dup".to_string(),
                };
                inner.captured.lock().await.push(tag);
            }
            inner.sessions.lock().await.remove(&client_id);
        });

        Ok(Response::new(
            Box::pin(ReceiverStream::new(out_rx).map(Ok)) as Self::ControlStream
        ))
    }

    type RequestDataStream = BoxStream<DataFrame>;

    async fn request_data(
        &self,
        request: Request<Streaming<DataFrame>>,
    ) -> Result<Response<Self::RequestDataStream>, Status> {
        let mut inbound = request.into_inner();
        let (out_tx, out_rx) = mpsc::channel::<DataFrame>(64);
        let inner = self.inner.clone();

        tokio::spawn(async move {
            // First frame must be head for correlation.
            let head = match inbound.next().await {
                Some(Ok(DataFrame {
                    frame: Some(data_frame::Frame::Head(h)),
                })) => h,
                _ => return,
            };
            let mut pending = inner.pending.lock().await;
            let Some(mut p) = pending.remove(&head.request_id) else {
                let _ = out_tx
                    .send(frames::error(
                        "unknown_request",
                        "no pending assignment",
                        false,
                    ))
                    .await;
                return;
            };
            if p.fencing != head.fencing_token {
                let _ = out_tx
                    .send(frames::error(
                        "fencing_mismatch",
                        "stale fencing token",
                        false,
                    ))
                    .await;
                return;
            }
            drop(pending);

            // Forward the request payload as request_json frames.
            let payload = std::mem::take(&mut p.payload);
            if !payload.is_empty() {
                for chunk in payload.chunks(256 * 1024) {
                    if out_tx
                        .send(frames::request_json(chunk.to_vec()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            if let Some(ready) = p.ready_tx.take() {
                let _ = ready.send(());
            }

            while let Some(item) = inbound.next().await {
                match item {
                    Ok(DataFrame {
                        frame: Some(data_frame::Frame::ResponseChunk(b)),
                    }) => {
                        if p.resp_tx.send(b.into()).await.is_err() {
                            break;
                        }
                    }
                    Ok(DataFrame {
                        frame: Some(data_frame::Frame::End(e)),
                    }) => {
                        if let Some(done) = p.done_tx.take() {
                            let _ = done.send(if e.cancelled {
                                "cancelled".into()
                            } else {
                                e.finish_reason.clone()
                            });
                        }
                        break;
                    }
                    Ok(DataFrame {
                        frame: Some(data_frame::Frame::Error(e)),
                    }) => {
                        if let Some(done) = p.done_tx.take() {
                            let _ = done.send(format!("error:{}", e.code));
                        }
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });

        Ok(Response::new(
            Box::pin(ReceiverStream::new(out_rx).map(Ok)) as Self::RequestDataStream
        ))
    }

    async fn get_config(
        &self,
        request: Request<ConfigRequest>,
    ) -> Result<Response<ConfigSnapshot>, Status> {
        let req = request.into_inner();
        let creds = self.inner.accept_credentials.lock().await.clone();
        if !creds.is_empty() && !creds.contains(&req.credential) {
            return Err(Status::unauthenticated("bad credential"));
        }
        Ok(Response::new(ConfigSnapshot {
            config_version: 0,
            desired_config_json: b"{}".to_vec(),
        }))
    }
}
