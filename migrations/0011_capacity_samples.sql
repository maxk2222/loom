-- Periodic fleet-capacity snapshots for the "capacity over time" charts.
-- One row per deployment per sampler tick (~20 s). Raw gauges are stored —
-- percentages are derived at query time so historical reads stay correct
-- when budgets/configs change afterwards (e.g. a larger vLLM KV pool).
-- `live`/`kv_gated` freeze the scheduler-view flags at sample time so old
-- rows replay through the same filter the dashboard applies today.
CREATE TABLE capacity_samples (
    ts            TIMESTAMPTZ NOT NULL,
    deployment_id UUID NOT NULL,
    client_id     UUID,
    kv_used       BIGINT NOT NULL DEFAULT 0,
    kv_budget     BIGINT NOT NULL DEFAULT 0,
    slots_total   INT NOT NULL DEFAULT 0,
    slots_free    INT NOT NULL DEFAULT 0,
    state         TEXT NOT NULL DEFAULT '',
    health        TEXT NOT NULL DEFAULT '',
    kv_gated      BOOLEAN NOT NULL DEFAULT false,
    live          BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX capacity_samples_ts_idx ON capacity_samples (ts);
COMMENT ON TABLE capacity_samples IS
    'Fleet capacity gauge snapshots (~20 s cadence). kv_budget is the EFFECTIVE budget at sample time; live = enabled && state=available && health!=down; kv_gated = live && budget>0 && admission!=slots.';
