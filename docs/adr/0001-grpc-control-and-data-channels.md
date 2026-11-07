# ADR-0001: Client-initiated gRPC control stream + per-request data streams

## Status

Accepted (Phase 0)

## Context

LOOM needs a bidirectional channel from central server to workstation
clients. Constraints:

- Clients sit behind NAT/firewalls on research workstations — only
  client-initiated connections work.
- Large request bodies (multimodal prompts) must not block heartbeat or
  control traffic.
- Per-request backpressure is required so slow callers don't starve other
  streams.
- Cancellation must propagate server→client→backend.

## Decision

Single persistent `Control` bidirectional stream for control messages
(< 64 KiB each), plus a dedicated bidirectional `RequestData` stream per
assignment opened by the client after accepting `AssignRequest`.

Alternatives considered:

- **One multiplexed stream for everything** — head-of-line blocking; a 4 MiB
  body delays heartbeats and cancels. Rejected.
- **HTTP long-poll / SSE + POST back** — two connections, higher latency,
  more complex correlation. Rejected.
- **WebSocket** — no native flow control, weaker typing. Rejected.

## Consequences

- Control latency is protected; payload flows independently.
- Correlation requires `request_id` + `fencing_token` in the data-stream
  `head` frame.
- Slightly more code (two RPC handlers), but each stays simple and testable.
- Protobuf RPC renamed `Connect` → `Control` because tonic generates a
  conflicting `connect()` client constructor.
