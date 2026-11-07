//! SSE event bus for dashboard live updates.

use tokio::sync::broadcast;

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<String>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(512);
        Self { tx }
    }

    pub fn publish(&self, kind: &str, payload: &serde_json::Value) {
        let _ = self.tx.send(format!(
            "event: {kind}\ndata: {}\n\n",
            serde_json::to_string(payload).unwrap_or_default()
        ));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
