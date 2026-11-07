# LOOM Protocol Specification (loom.agent.v1)

This document specifies the client↔server wire protocol defined in `proto/agent.proto`.

## Topology

```
workstation (client)                         server
┌───────────────────┐  TLS / HTTP2           ┌─────────────────────┐
│ loom-client       │ ─── Control (bidi) ──▶ │ loom-server gRPC    │
│  local API :8391  │ ─── RequestData (bidi) │  (per assignment)   │
│  backends (vLLM,  │ ─── GetConfig          │                     │
│   Ollama, OAI)    │ ◀── server pushes ───  │                     │
└───────────────────┘                        └─────────────────────┘
```

All connections are **client-initiated**. The server never opens connections
to clients or to inference backends.

## Services

### `rpc Control(stream ClientEnvelope) returns (stream ServerEnvelope)`

The persistent control channel. Exactly one per client session.

#### Session establishment

1. Client opens the stream and sends `Hello` first:
   - `protocol_version`, `protocol_version_min` — semver-ish ints; server
     accepts iff `protocol_version >= server_min` and `protocol_version_min <= server_max`.
   - `credential` — either an enrollment token (first connect, `client_id` empty)
     or the stored device credential.
   - `capabilities`: supported request-content features.
   - `inventory`: known deployments/capacities (recovery after restart).
2. Server replies `HelloAck`:
   - `accepted=false` → close + abort (reason in `error`).
   - On success returns `session_id`, assigned `client_id` and, after
     enrollment, a one-time `device_credential` that the client must persist
     (the enrollment token is burned).
3. Heartbeat: client sends `Heartbeat{session_id}` every `hb_interval_secs`
   (default 10 s, server may override). Server marks the client offline after
   3 missed heartbeats.

#### Config push

Server sends `ConfigPush{config_version, desired_config_json}`. Client applies
idempotently and replies `ConfigAck{config_version, applied, error}`.

#### Request assignment

1. Server sends `AssignRequest{request_id, lease_id, fencing_token,
   deadline_unix_ms, api, required_caps, payload_bytes, model, deployment_id}`.
   **Metadata only** — bodies never traverse the control stream.
2. Client admits via its local scheduler and replies `AssignAck`:
   `accepted` → proceed, `reject{reason}` → server tries the next candidate.
3. On accept the client opens a `RequestData` stream for this `request_id`.

#### Completion

Client sends `RequestEnd{request_id, status, error_code, prompt_tokens,
completion_tokens, ttft_ms, duration_ms}` once per request.

`CancelRequest{request_id, fencing_token}` cancels an active request; the
client only cancels if the fencing token matches its active request.

### `rpc RequestData(stream DataFrame) returns (stream DataFrame)`

Bidirectional per-request payload channel, opened by the client after
`AssignAck.accepted`.

- First frame from client: `head{request_id, lease_id, fencing_token}`.
  Server validates fencing; unknown/stale → `error{code:"unknown_request"}`
  + close.
- Server → client: `request_json` frames carrying the OpenAI request body,
  chunked at ≤ 1 MiB (default total bound 4 MiB enforced by the gateway).
- Client → server: `response_chunk` frames (SSE event bytes for streaming
  APIs or the JSON document for unary APIs), then exactly one terminal frame:
  `end{}` (success) or `error{code,message,retryable}`.
- Either side closing the stream aborts the request.

### `rpc GetConfig(ConfigRequest) returns (ConfigSnapshot)`

Stateless pull for reconciliation after reconnect: server returns the current
`config_version` and `desired_config_json` for that client.

## Message bounds

| Message                    | Default limit |
|----------------------------|---------------|
| Control envelope           | 64 KiB        |
| Data frame                 | 1 MiB         |
| Aggregated request payload | 4 MiB         |

## Fencing

`cap:fencing` in Redis is a monotonically increasing counter incremented on
every lease. `AssignRequest`, `CancelRequest` and `RequestData.head` carry the
token. Any message presenting a token that does not match the current active
request is stale and must be rejected/ignored. This prevents double-execution
after reconnect, lease expiry and dispatcher retries.

## Reconnect & session recovery

- On reconnect the client sends `Hello` with its stored `client_id` +
  device credential and an `inventory` report.
- Server compares `config_version` — if the client's is stale, it pushes a
  fresh `ConfigPush` (or the client calls `GetConfig`).
- Leases held by a client that goes offline are resolved atomically:
  `reserved` leases requeue their request, `active` leases fail the request.

## Queue semantics (server-side)

- Requests enter a per-priority-class queue when no member has free
  tier-2 capacity, keeping their original FIFO position across requeues.
- Shortage kinds are distinguished: a member being **busy** is transient
  (request waits until its deadline); **all members unreachable** is
  structural — the request is counted against `max_attempts` and then
  fails `unavailable`, so it cannot head-of-line block the class.
- Client-visible `429` responses include `Retry-After`
  (`1s` concurrency, `60s / rpm_limit` for rate windows).

## Error codes (data `error` frames / RequestEnd)

`admission_rejected`, `capacity_decreased`, `backend_unavailable`,
`backend_error`, `timeout`, `cancelled`, `client_offline`, `protocol_error`,
`internal` — plus `retryable` flag for retry hints.
