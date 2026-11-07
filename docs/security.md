# LOOM Security Model

## Threat model

LOOM connects untrusted networks (lab workstations ↔ central server) for LLM
inference. Assets: model outputs (may contain research data), API keys,
device credentials, prompt contents, client compute capacity.

## Authentication & identity

| Actor | Credential | Storage |
|-------|-----------|---------|
| API caller (`/v1/*`) | `sk-…` API key | SHA-256 hash + display prefix in Postgres |
| Workstation client | `enroll_…` one-time token → `dev_…` device credential | SHA-256 hash in Postgres |
| Dashboard user | email + password | Argon2 hash in Postgres |
| Dashboard session | `loom_session` cookie | HttpOnly, SameSite=Lax, 7 d, token hash in Postgres |

- Enrollment tokens are single-use (configurable `max_uses`) and expiring.
- Device credentials are rotatable and revocable (`clients.status`).
- Suspended clients cannot authenticate at the gRPC endpoint.

## Transport

- `/v1/*` + `/api/*` + dashboard: HTTPS via Caddy (or terminated LB).
- gRPC agent endpoint: TLS (server cert) in production; mTLS-ready credential
  abstraction — a device credential is required regardless of TLS mode.
- Local client API: binds `127.0.0.1` only; optional local bearer token.

## Authorization

- API keys carry scopes (`chat`, `completions`, `embeddings`, `models`),
  per-key RPM/TPM/concurrency limits, project priority class.
- Model access: key `allowed_models` list, else project grants
  (`project_models`). Denied models return `model_not_found` (404) — no
  existence leak beyond what an allowed caller could infer.
- Dashboard roles: `admin` (full), `operator` (read + lifecycle ops),
  `viewer` (read-only) — enforced at handler level.

## Data protection

- **Prompts and completions are never persisted** — not in Postgres, Redis,
  logs or metrics. `requests_history` stores metadata only (timings, token
  counts, status, routing trace).
- Redis holds queue/lease/rate-limit metadata only; request bodies live in
  server memory and are dropped on restart.
- Logo uploads restricted to PNG/JPG/WebP, ≤ 2 MB, admin-only — no SVG/XSS.
- Secrets via env vars / `enroll.env` (mode 600) — never committed.
- gRPC control messages ≤ 64 KiB, data frames ≤ 1 MiB, request bodies
  ≤ 4 MiB at the gateway — bounded resource usage.

## Isolation invariants

- The server **never connects to inference backends** — `endpoint_url` in
  `deployments` is metadata consumed by the client only.
- Local-first admission: client-side admission is authoritative; the server
  cannot force a request onto a workstation.

## Rate limiting & abuse

- Sliding-window RPM (Redis), concurrency counters, best-effort TPM.
- Global queue cap → `overloaded` 429, protecting backends and memory.
- Per-request deadlines; queue timeouts; bounded retries.

## Operational hardening

- systemd unit: dedicated user, `NoNewPrivileges`, `ProtectSystem=strict`,
  writable path limited to data dir.
- Compose binds Postgres/Redis to `127.0.0.1` by default; server container
  runs as non-root (uid 10001).
- Security headers via Caddy (nosniff, DENY framing, no Server header).

## Known limitations / roadmap

- mTLS not yet enabled by default (TLS + device credential today).
- Audit log records admin actions; request-level audit intentionally omits
  content for privacy.
- Update signing via minisign planned for release artifacts (updater already
  verifies SHA-256 checksums over HTTPS).
