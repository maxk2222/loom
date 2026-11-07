-- Admin notes/briefings pushed to client dashboards over the control stream.
-- client_id NULL = broadcast to every client.
CREATE TABLE IF NOT EXISTS client_notes (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    client_id   uuid REFERENCES clients(id) ON DELETE CASCADE,  -- NULL = all clients
    title       text NOT NULL,
    body        text NOT NULL DEFAULT '',
    severity    text NOT NULL DEFAULT 'info'
                CHECK (severity IN ('info','warning','critical')),
    persistent  boolean NOT NULL DEFAULT false,  -- cannot be dismissed client-side
    expires_at  timestamptz,                     -- NULL = never
    created_by  text NOT NULL DEFAULT '',
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS client_notes_client_idx ON client_notes (client_id);
