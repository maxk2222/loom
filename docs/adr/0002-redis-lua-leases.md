# ADR-0002: Atomic scheduling state in Redis via Lua scripts

## Status

Accepted (Phase 0)

## Context

The tier-2 scheduler needs: priority class queues, per-deployment capacity
leases, fencing tokens, request lifecycle metadata and rate-limit state —
all shared, all race-prone. Multiple server replicas must behave correctly.

## Decision

All mutable scheduling state lives in Redis under one key prefix, mutated
exclusively through Lua scripts:

- `queue_enqueue` / `queue_pop` / `queue_remove` — class FIFO with global cap
  and watchdog scores.
- `lease_acquire` / `lease_confirm` / `lease_activate` / `lease_release` /
  `lease_sweep` — atomic slot counting + fencing (`cap:fencing` INCR).
- `client_offline` — resolves every lease of a dead client in one shot.
- `rl_check` / `rl_commit` / `inflight_dec` — rate limits + inflight counters.

Request **bodies are never stored in Redis** — only metadata. Bodies live in
server memory; a restart fails in-flight/queued requests with controlled
errors (documented behavior, keeps Redis small and avoids prompt leakage).

## Alternatives

- Postgres for queue/leases — too slow for hot scheduling loops, row locks.
- In-memory only — breaks multi-replica and restart semantics.
- Optimistic WATCH/MULTI — more round trips, same atomicity.

## Consequences

- Deterministic, single-round-trip atomicity for every state transition.
- Lua must be kept simple and bounded (no unbounded loops over request sets).
- Boot reconciliation wipes runtime keys on server start; clients resync.
