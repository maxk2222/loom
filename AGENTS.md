# LOOM — Build, Test, Dev Commands

## Workspace
Rust monorepo. Binaries: `loom-server`, `loom-client`, `loom` (CLI). Shared crates: `loom-core`, `loom-protocol`, `loom-testkit`.

## Build & Check
```bash
cargo build --workspace
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Tests
```bash
cargo test --workspace                       # unit tests (no external deps)
# integration tests need Postgres+Redis (docker compose):
docker compose -f deploy/docker-compose.yml up -d postgres redis
LOOM_TEST_DATABASE_URL=postgres://loom:loom@localhost:5432/loom_test \
LOOM_TEST_REDIS_URL=redis://localhost:6379/15 \
  cargo test -p loom-testkit
```

## Dev services
```bash
docker compose -f deploy/docker-compose.yml up -d postgres redis
```

## Run (dev)
```bash
cargo run -p loom-server            # gateway :8080, grpc :50051, ui /ui
cargo run -p loom-client            # local api + ui on 127.0.0.1:8391
```

## Frontend
```bash
cd frontend
npm install
npm run dev                        # vite dev server, proxies to :8080
npm run typecheck && npm run test && npm run build
```

## Proto
```bash
buf lint proto/                    # if buf installed; otherwise build via cargo (protoc vendored)
```

## Conventions
- No prompt/response bodies in Redis, logs, metrics, or requests_history — metadata only.
- Server NEVER connects to backend endpoints; inference data flows only over the client-initiated gRPC channel.
- All scheduling mutations via Redis Lua scripts (atomic).
- Request bodies live only in server memory; queued entries carry metadata only.
- Dark-only UI. English everywhere (code, docs, UI, commits).
