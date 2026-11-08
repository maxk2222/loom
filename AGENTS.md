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

Agent-channel TLS/mTLS (production-style, all outbound client-initiated):
```bash
# listeners: strict mTLS + cert-issuance bootstrap + optional plaintext migration
LOOM_GRPC_BIND=0.0.0.0:50052 LOOM_GRPC_TLS_CERT=…/server.crt \
  LOOM_GRPC_TLS_KEY=…/server.key LOOM_MTLS=1 \
  LOOM_GRPC_BOOTSTRAP_BIND=0.0.0.0:50053 LOOM_GRPC_PLAIN_BIND=0.0.0.0:50051 \
  cargo run -p loom-server
# internal CA lives at $LOOM_DATA_DIR/tls/ca.{crt,key} (auto-created/reused);
# clients auto-issue+renew certs via CSR in Hello — config.toml only needs
# connector.tls="require" + ca_cert_path; leave client_cert_path empty.
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

## Deployment (always via updater, never manual binary copy)

Clients self-update from signed GitHub release artifacts. After a tag
release completes (`gh run list --workflow release`), push updates from
the admin dashboard (Clients → Update) or API:

```bash
# per client
curl -X POST http://server:8080/api/clients/<id>/update -H "Cookie: loom_session=$TOK"
# client-side check/apply
curl http://127.0.0.1:8391/local/api/update-check
curl -X POST http://127.0.0.1:8391/local/api/update
```

Building x86_64 binaries from Apple Silicon requires an explicit
platform (otherwise aarch64 → `203/EXEC` on the nodes):
`docker build --platform linux/amd64 ...`

## Conventions
- No prompt/response bodies in Redis, logs, metrics, or requests_history — metadata only.
- Server NEVER connects to backend endpoints; inference data flows only over the client-initiated gRPC channel.
- All scheduling mutations via Redis Lua scripts (atomic).
- Request bodies live only in server memory; queued entries carry metadata only.
- Certs are transport-level fleet proof; the device credential still gates session identity. Cert CN/SAN is always bound to the *authenticated* client_id, never the CSR's claimed DN.
- rustls: call `rustls::crypto::aws_lc_rs::default_provider().install_default()` at process start (dep tree has both providers — without it rustls panics on first TLS config).
- Dark-only UI. English everywhere (code, docs, UI, commits).
