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
  LOOM_GRPC_BOOTSTRAP_BIND=0.0.0.0:50053 LOOM_GRPC_PLAIN_BIND=127.0.0.1:50054 \
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

Releases are tag-triggered (`git tag vX.Y.Z && git push --tags`). The
workflow produces signed binaries for linux x86_64/aarch64, macOS aarch64
and Windows, **plus GHCR images** (`ghcr.io/<owner>/loom-{server,client}:X.Y.Z`)
built FROM those same signed binaries — identical bits on every path.

Update matrix — pick by component:

| Component | Canonical update path |
|---|---|
| Server (docker) | `POST /api/server/update` (admin session) — downloads the signed binary, swaps it into `/data/bin`, exits; restart policy brings it up. `{"version":"x.y.z"}` pins a release (downgrade); `GET /api/server/update-status` reports the outcome after restart |
| Client (any)   | Dashboard → Clients → Update, or `POST /api/clients/<id>/update`, or client-local `POST :8391/local/api/update` |
| Compose stack  | Pin `LOOM_IMAGE_TAG=X.Y.Z` (image `ghcr.io/<owner>/loom-server`) and `docker compose pull && up -d` — for rebuilds/fresh installs, not rolling updates |

Why both work: container entrypoints seed the binary into the state volume
(`/data/bin`, `/etc/loom-client/bin`) and exec it from there. On each start
the version guard decides: image version > volume → re-seed; same version
newer `/etc/loom-build-id` → re-seed; volume version > image → keep
(self-updated binary survives `--force-recreate`). Swaps are atomic renames.

```bash
# per client (server-pushed over the live control stream)
curl -X POST http://server:8080/api/clients/<id>/update -H "Cookie: loom_session=$TOK"
# client-side check/apply
curl http://127.0.0.1:8391/local/api/update-check
curl -X POST http://127.0.0.1:8391/local/api/update
```

Rollback runbook:
- **Crash after self-update**: automatic — the server writes
  `<binary>.boot-ok` naming its version once all listeners bind; the
  entrypoint restores `.prev` when the running binary never proved itself
  but `.prev` IS the proven one. Manual fallback unchanged:
  `cp loom-server.prev loom-server && docker restart loom-server-1`.
- **Intentional downgrade** (pinning an older image tag alone does NOT
  downgrade — the version guard protects the volume binary): set
  `LOOM_FORCE_RESEED=1` on the service, pin the older image tag, `docker
  compose up -d`, then remove the env var.
- Self-update is recreate-safe but NOT image-rollback-safe by design —
  always prefer `LOOM_FORCE_RESEED` over deleting files on the volume.

Instance-owner lock (`crates/loom-server/src/instance_lock.rs`): exactly
one server per Redis namespace may `reset_runtime` at boot. The owner
CAS-deletes `instance:owner` on SIGTERM/SIGINT and before the
self-update exit, so restarts hand over instantly. A dead predecessor
(no release — SIGKILL/crash) is waited out via its remaining lock TTL
(`LOOM_INSTANCE_LOCK_WAIT_MS`, default 35000; must stay < ~50s
healthcheck threshold, `0` = single-shot). A live peer is detected by
its rising PTTL and skipped fast — multi-instance unchanged.

New-client install is one command: the gateway serves `deploy/install-client.sh`
at `GET /install-client.sh` with the `scope=public` URL from the `endpoint_hints`
setting (`public_base` fallback) substituted as `SERVER_URL`. The script resolves
the latest GitHub release, detects arch, sha256-verifies, installs client+CLI,
and writes config/token/systemd-or-launchd unit.

Building x86_64 binaries from Apple Silicon requires an explicit
platform (otherwise aarch64 → `203/EXEC` on the nodes):
`docker build --platform linux/amd64 ...`

## Queue priority semantics
- Requests carry a class `0..priority_classes-1` (config `priority_classes`,
  default 4). Class 0 is highest; queues pop strictly high→low, FIFO in-class.
- Effective class = `COALESCE(api_keys.priority_class, projects.priority_class)`;
  key NULL inherits the project. Admin API rejects out-of-range values (400);
  `Scheduler::submit` clamps defensively anyway (unpopped-queue tomb guard).
- Fast-path rule: a request may skip the queue only when `queue:{0..=prio}`
  are ALL empty (`higher_or_equal_empty`) — never let a low-class submit race
  ahead of queued high-class work. Queued lower-class work never blocks a
  higher-class submit.
- Tier-1 local client traffic never enters this queue — it outranks all
  classes by architecture.
- `projects.weight` exists but is intentionally unused (reserved knob — no UI).

## Conventions
- No prompt/response bodies in Redis, logs, metrics, or requests_history — metadata only.
- Server NEVER connects to backend endpoints; inference data flows only over the client-initiated gRPC/WS channel.
- Agent channel has two transports: tonic gRPC for `http(s)://` endpoints,
  protobuf-over-WebSocket for `ws(s)://` endpoints (HTTP gateway routes
  `/api/agent/ws/control`, `/api/agent/ws/data`, `/api/agent/config` —
  needed through buffered HTTP/2 proxies like cloudflared without QUIC).
  Both share `control_stream`/`request_data_stream`/`config_snapshot` in
  `loom-server/src/grpc_service.rs`; never duplicate session semantics.
- All scheduling mutations via Redis Lua scripts (atomic).
- Request bodies live only in server memory; queued entries carry metadata only.
- Certs are transport-level fleet proof; the device credential still gates session identity. Cert CN/SAN is always bound to the *authenticated* client_id, never the CSR's claimed DN.
- rustls: call `rustls::crypto::aws_lc_rs::default_provider().install_default()` at process start (dep tree has both providers — without it rustls panics on first TLS config).
- Dark-only UI. English everywhere (code, docs, UI, commits).
