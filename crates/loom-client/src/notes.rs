//! Admin notes/briefings pushed from the server over the control stream.
//!
//! The server sends a `NotesSync` (full authoritative list for this client)
//! on connect and whenever notes change. The client keeps the set in memory
//! and persists it to `notes.json` next to the config so notices survive
//! restarts. Dismissal is client-local: non-persistent notes can be hidden,
//! persistent briefings cannot.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredNote {
    pub id: String,
    pub title: String,
    pub body: String,
    /// "info" | "warning" | "critical"
    pub severity: String,
    /// Server-pinned briefing — cannot be dismissed client-side.
    #[serde(default)]
    pub persistent: bool,
    pub created_at_unix: i64,
    /// 0 = never expires.
    #[serde(default)]
    pub expires_at_unix: i64,
    /// Local-only flag, never sent back to the server.
    #[serde(default)]
    pub dismissed: bool,
}

pub type SharedNotes = Arc<RwLock<Vec<StoredNote>>>;

pub fn load_notes(path: &Path) -> SharedNotes {
    let items: Vec<StoredNote> = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    Arc::new(RwLock::new(items))
}

pub fn save_notes(path: &Path, notes: &SharedNotes) {
    let items: Vec<StoredNote> = notes.read().unwrap().clone();
    let Ok(json) = serde_json::to_vec(&items) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Replace the stored set with the server's authoritative list, keeping
/// local dismissal flags for ids that still exist. Expired notes are
/// dropped. Returns true when the visible set changed.
pub fn apply_sync(notes: &SharedNotes, incoming: Vec<loom_protocol::pb::ClientNote>) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut w = notes.write().unwrap();
    let dismissed: std::collections::HashSet<String> = w
        .iter()
        .filter(|n| n.dismissed)
        .map(|n| n.id.clone())
        .collect();
    let next: Vec<StoredNote> = incoming
        .into_iter()
        .filter(|n| n.expires_at_unix == 0 || n.expires_at_unix > now)
        .map(|n| {
            let was_dismissed = dismissed.contains(&n.id);
            StoredNote {
                id: n.id,
                title: n.title,
                body: n.body,
                severity: n.severity,
                persistent: n.persistent,
                created_at_unix: n.created_at_unix,
                expires_at_unix: n.expires_at_unix,
                dismissed: was_dismissed,
            }
        })
        .collect();
    let changed = *w != next;
    *w = next;
    changed
}

/// Mark a note dismissed. Persistent notes refuse and return false.
pub fn dismiss(notes: &SharedNotes, id: &str) -> bool {
    let mut w = notes.write().unwrap();
    if let Some(n) = w.iter_mut().find(|n| n.id == id && !n.persistent) {
        n.dismissed = true;
        true
    } else {
        false
    }
}
