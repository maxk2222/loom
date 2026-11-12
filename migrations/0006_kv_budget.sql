-- KV-token budgeted admission: per-deployment shared KV budget in tokens.
-- NULL = feature off (pure slot counting, identical to previous behavior).
-- The scheduler admits a tier-2 lease only while
--   dep.kv_used + request_footprint <= kv_budget_tokens
-- alongside the existing slot gate (tier2_free).
ALTER TABLE deployments
    ADD COLUMN IF NOT EXISTS kv_budget_tokens BIGINT,
    -- Client-reported KV capacity (adapter-derived); used as the effective
    -- budget when kv_budget_tokens is NULL: COALESCE(override, reported).
    ADD COLUMN IF NOT EXISTS reported_kv_budget_tokens BIGINT;
