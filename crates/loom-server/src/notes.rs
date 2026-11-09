//! Admin notes/briefings: Postgres-persisted, pushed to clients over their
//! control streams as a `NotesSync` (full authoritative set per client).
//!
//! Delivery points: on client connect (grpc_service) and whenever an admin
//! writes/deletes a note (admin). Offline clients simply get the current set
//! on their next connect — no ack tracking needed.

use loom_protocol::pb::ClientNote;
use loom_protocol::server_msgs;
use sqlx::PgPool;

type NoteRow = (
    uuid::Uuid,
    String,
    String,
    String,
    bool,
    chrono::DateTime<chrono::Utc>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// All live notes visible to one client (own + broadcast, not expired).
pub async fn notes_for_client(db: &PgPool, client_id: &str) -> Vec<ClientNote> {
    let rows: Vec<NoteRow> = sqlx::query_as(
        "SELECT id, title, body, severity, persistent, created_at, expires_at
         FROM client_notes
         WHERE (client_id = $1::uuid OR client_id IS NULL)
           AND (expires_at IS NULL OR expires_at > now())
         ORDER BY created_at DESC",
    )
    .bind(client_id)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(
            |(id, title, body, severity, persistent, created, expires)| ClientNote {
                id: id.to_string(),
                title,
                body,
                severity,
                persistent,
                created_at_unix: created.timestamp(),
                expires_at_unix: expires.map(|e| e.timestamp()).unwrap_or(0),
            },
        )
        .collect()
}

/// Push the current note set to one connected client (no-op when offline).
pub async fn push_to(sessions: &crate::session::SessionManager, db: &PgPool, client_id: &str) {
    let notes = notes_for_client(db, client_id).await;
    let _ = sessions
        .send(client_id, server_msgs::notes_sync(notes))
        .await;
}

/// Push to every connected client — used after any note write so all views
/// converge (broadcast adds, scoped adds and deletes all covered).
pub async fn push_all(sessions: &crate::session::SessionManager, db: &PgPool) {
    for (id, _) in sessions.connected_ids().await {
        push_to(sessions, db, &id).await;
    }
}
