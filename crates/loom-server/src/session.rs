//! Session manager: live client control streams + per-request data streams.
//!
//! - `register`: stores the control sender for a client_id.
//! - `dispatch`: sends ServerEnvelope to a client.
//! - `await_data`: correlates an incoming RequestData stream (head frame) to a
//!   pending assignment.

use futures_util::Stream;
use loom_protocol::pb::{DataFrame, ServerEnvelope};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

/// Inbound client data stream: `tonic::Streaming` on gRPC transports,
/// a decoded WebSocket adapter stream on the WS bridge.
type InboundData = Pin<Box<dyn Stream<Item = Result<DataFrame, tonic::Status>> + Send>>;
type DataStreamPair = (InboundData, mpsc::Sender<DataFrame>);

#[derive(Default)]
pub struct SessionManager {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// client_id -> (session_id, control stream sender). The session_id is
    /// stored so a lingering duplicate stream cannot evict the live session's
    /// entry when it finally dies.
    sessions: HashMap<String, (String, mpsc::Sender<ServerEnvelope>)>,
    /// client_id -> (session_id, kick handle): firing it makes the control
    /// loop exit through the normal disconnect path (leases requeued/failed,
    /// client marked offline). Used for admin suspension so revocation takes
    /// effect on the live session, not just on the next reconnect.
    kicks: HashMap<String, (String, oneshot::Sender<()>)>,
    /// request_id -> correlator for the incoming RequestData stream pair:
    /// (inbound client stream, outbound server->client sender).
    pending_data: HashMap<String, oneshot::Sender<DataStreamPair>>,
}

impl SessionManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub async fn register(
        &self,
        client_id: &str,
        session_id: &str,
        tx: mpsc::Sender<ServerEnvelope>,
    ) {
        let mut inner = self.inner.lock().await;
        inner
            .sessions
            .insert(client_id.to_string(), (session_id.to_string(), tx));
        crate::metrics::clients_online(inner.sessions.len() as f64);
    }

    /// Remove a session only when it is still the registered one — a stale
    /// duplicate stream dying late must not evict the live session's sender.
    pub async fn unregister(&self, client_id: &str, session_id: &str) {
        let mut inner = self.inner.lock().await;
        if inner
            .sessions
            .get(client_id)
            .is_some_and(|(sid, _)| sid == session_id)
        {
            inner.sessions.remove(client_id);
            inner.kicks.remove(client_id);
            crate::metrics::clients_online(inner.sessions.len() as f64);
        }
    }

    /// Arm a kick handle for a session; the control loop should select on the
    /// returned receiver and break when it resolves.
    pub async fn register_kick(&self, client_id: &str, session_id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .await
            .kicks
            .insert(client_id.to_string(), (session_id.to_string(), tx));
        rx
    }

    /// Force a live session into its disconnect path. Returns false when the
    /// client is not connected or was already kicked.
    pub async fn kick(&self, client_id: &str) -> bool {
        if let Some((_, tx)) = self.inner.lock().await.kicks.remove(client_id) {
            let _ = tx.send(());
            true
        } else {
            false
        }
    }

    pub async fn send(&self, client_id: &str, msg: ServerEnvelope) -> Result<(), &'static str> {
        // Clone the sender under a short lock — awaiting a full channel while
        // holding the session map would wedge every session operation.
        let tx = {
            let inner = self.inner.lock().await;
            inner.sessions.get(client_id).map(|(_, tx)| tx.clone())
        };
        let Some(tx) = tx else {
            return Err("client not connected");
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), tx.send(msg))
            .await
            .map_err(|_| "control stream slow")?
            .map_err(|_| "control stream closed")
    }

    pub async fn connected(&self, client_id: &str) -> bool {
        self.inner.lock().await.sessions.contains_key(client_id)
    }

    /// (client_id, session_id) pairs for every live session.
    pub async fn connected_ids(&self) -> Vec<(String, String)> {
        self.inner
            .lock()
            .await
            .sessions
            .iter()
            .map(|(cid, (sid, _))| (cid.clone(), sid.clone()))
            .collect()
    }

    /// Register expectation of a client data stream for a request.
    pub async fn expect_data(&self, request_id: &str) -> oneshot::Receiver<DataStreamPair> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .await
            .pending_data
            .insert(request_id.to_string(), tx);
        rx
    }

    /// Called by the gRPC service when a RequestData stream arrives with a
    /// validated head frame. `outbound` is the sender feeding the server's
    /// response stream back to the client.
    pub async fn deliver_data(
        &self,
        request_id: &str,
        inbound: InboundData,
        outbound: mpsc::Sender<DataFrame>,
    ) -> bool {
        let mut inner = self.inner.lock().await;
        if let Some(tx) = inner.pending_data.remove(request_id) {
            let _ = tx.send((inbound, outbound));
            true
        } else {
            false
        }
    }

    pub async fn drop_expectation(&self, request_id: &str) {
        self.inner.lock().await.pending_data.remove(request_id);
    }
}
