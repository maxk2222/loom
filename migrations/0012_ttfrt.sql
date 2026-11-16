-- requests_history.ttfrt_ms: time to first reasoning token, recorded
-- alongside ttft_ms (first non-reasoning output). 0 = no reasoning was
-- emitted; reasoning precedes the answer, so 0 < ttfrt_ms <= ttft_ms when
-- both are set. Rows written before this column existed stay at 0 and are
-- excluded from percentile aggregates via NULLIF.
ALTER TABLE requests_history
    ADD COLUMN ttfrt_ms BIGINT NOT NULL DEFAULT 0;
COMMENT ON COLUMN requests_history.ttfrt_ms IS
    'Milliseconds from request start to the first reasoning token (0 = none). Reasoning streams before answer content, so ttfrt_ms <= ttft_ms when both are set.';
