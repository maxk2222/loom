//! Session manager: live client control streams + per-request data streams.
//!
//! - `register`: stores the control sender for a client_id.
//! - `dispatch`: sends ServerEnvelope to a client.
//! - `await_data`: correlates an incoming RequestData stream (head frame) to a
//!   pending assignment.

use loom_protocol::pb::{DataFrame, ServerEnvelope};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tonic::Streaming;

#[derive(Default)]
pub struct SessionManager {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// client_id -> control stream sender.
    sessions: HashMap<String, mpsc::Sender<ServerEnvelope>>,
    /// request_id -> correlator for the incoming RequestData stream pair:
    /// (inbound client stream, outbound server->client sender).
    pending_data:
        HashMap<String, oneshot::Sender<(Streaming<DataFrame>, mpsc::Sender<DataFrame>)>>,
}

impl SessionManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub async fn register(&self, client_id: &str, tx: mpsc::Sender<ServerEnvelope>) {
        self.inner
            .lock()
            .await
            .sessions
            .insert(client_id.to_string(), tx);
    }

    pub async fn unregister(&self, client_id: &str) {
        self.inner.lock().await.sessions.remove(client_id);
    }

    pub async fn send(&self, client_id: &str, msg: ServerEnvelope) -> Result<(), &'static str> {
        let inner = self.inner.lock().await;
        let Some(tx) = inner.sessions.get(client_id) else {
            return Err("client not connected");
        };
        tx.send(msg).await.map_err(|_| "control stream closed")
    }

    pub async fn connected(&self, client_id: &str) -> bool {
        self.inner.lock().await.sessions.contains_key(client_id)
    }

    pub async fn connected_ids(&self) -> Vec<String> {
        self.inner.lock().await.sessions.keys().cloned().collect()
    }

    /// Register expectation of a client data stream for a request.
    pub async fn expect_data(
        &self,
        request_id: &str,
    ) -> oneshot::Receiver<(Streaming<DataFrame>, mpsc::Sender<DataFrame>)> {
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
        inbound: Streaming<DataFrame>,
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
