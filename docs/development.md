# Development Guide

## Layout

```
crates/
  loom-core       shared types: OpenAI API, capabilities, errors, config, ids
  loom-protocol   protobuf codegen + helpers (agent.v1)
  loom-client     workstation daemon
  loom-server     central server (gateway, scheduler, admin, gRPC)
  loom-cli        `loom` CLI
  loom-testkit    mock backend, mock agent server, test harness
proto/agent.proto wire protocol
migrations/       Postgres schema (sqlx migrate)
frontend/         React dashboard (server + client variants)
deploy/           compose, Dockerfile, installer, systemd, Caddy
tests/            integration tests
docs/             ADRs, protocol spec, security, runbooks
```

## Prerequisites

Rust ≥ 1.85, Node ≥ 22, Docker (for Postgres/Redis), `buf` (optional lint).

## Dev loop

```bash
# Infra
docker compose -f deploy/docker-compose.yml up -d postgres redis

# Server
export LOOM_DATABASE_URL=postgres://loom:loom@localhost:5432/loom
export LOOM_REDIS_URL=redis://localhost:6379/0
cargo run -p loom-server

# Dashboard (dev server, proxies /api + /v1 to :8080)
cd frontend && npm install && npm run dev        # → http://localhost:5173

# Client (against mock or real server)
cargo run -p loom-client
```

## Testing

```bash
cargo test --workspace                 # unit + integration without infra
LOOM_TEST_DATABASE_URL=postgres://loom:loom@localhost:5432/loom_test \
LOOM_TEST_REDIS_URL=redis://localhost:6379/1 \
cargo test --test '*'                  # infra-backed integration tests

cd frontend && npm run typecheck && npm run test && npm run build
```

## Conventions

- No prompts/responses in logs, metrics, Postgres or Redis — ever.
- All Redis mutations in `loom-server/src/lua.rs` scripts.
- New entities: migration → sqlx queries → admin REST → dashboard page.
- Frontend is dark-only; use `bg-panel`, `border-border`, `text-muted` tokens,
  `var(--accent)` for accent-colored elements.
- English only (code, comments, UI, docs).

## Adding a backend adapter

Implement `BackendAdapter` in `loom-client/src/adapters/mod.rs` (list_models,
probe_caps, health, forward, cancel), register in `backend_manager.rs`, add
`type` string in config parsing.

## Protocol changes

Edit `proto/agent.proto` → `cargo build -p loom-protocol` regenerates code.
Bump `PROTOCOL_VERSION` in `loom-protocol/src/lib.rs` on breaking changes.
