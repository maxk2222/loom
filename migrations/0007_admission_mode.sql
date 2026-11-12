-- Per-deployment admission mode: how tier-2 concurrency is bounded.
--   'auto'   (default) — token gate when a positive budget exists,
--            slot-only otherwise. Matches pre-column behaviour.
--   'slots'  — concurrency-only; the KV budget is ignored even when set.
--            Right for backends whose parallelism is fixed (Ollama
--            num_parallel, LM Studio instances) rather than KV-bound.
--   'tokens' — token gate REQUIRED: with no effective budget the
--            deployment admits nothing until one is reported or set.
--            Right for KV-pooled engines (vLLM GPU blocks) where slot
--            counts alone would over-admit and trigger context pressure.
ALTER TABLE deployments
    ADD COLUMN IF NOT EXISTS admission_mode TEXT NOT NULL DEFAULT 'auto';
