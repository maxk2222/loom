# ADR-0004: Local-first admission control on the client

## Status

Accepted (Phase 1)

## Context

LOOM shares spare workstation capacity with a central pool. The hard
invariant: **local research workloads must never be degraded by central
requests**.

## Decision

The client's local scheduler is the sole authority over its deployments:

- `max_concurrent` — total parallelism.
- `local_reserve` — slots never offered to the pool.
- `tier2_max` — upper bound of shareable slots.
- Sharing modes: `auto` (tier-2 shrinks to zero under local pressure),
  `fixed`, `manual`, `off`.

Local requests are **admitted or rejected with 429** — never queued locally
(v1). Tier-2 assignments arrive via `AssignRequest` and go through the same
admission; the client may reject at any time and the server retries the next
candidate. Server leases are *reservation requests*, not grants — validated
again locally at assignment time with fencing.

## Consequences

- Workstation owners keep full control; no central command can oversubscribe.
- Rejections are normal routing signals, not errors.
- Under local pressure the offered capacity collapses to zero automatically —
  the central scheduler simply sees less `tier2_free`.
