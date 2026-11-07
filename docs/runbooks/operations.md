# Operations Runbook

## Monitoring

- `/healthz` — liveness. `/readyz` — Postgres + Redis reachable.
- Prometheus: start server with `--metrics-bind 0.0.0.0:9090` → scrape `:9090/metrics`.
- Dashboard → Monitoring: TTFT p50/p99, failure counts, usage per project.
- Key metrics: `loom_requests_total{status}`, `loom_ttft_seconds`,
  `loom_queue_depth`, `loom_clients_online`.

## Common operations

### Suspend a client

Dashboard → Clients → Suspend. Its gRPC session stays until reconnect;
leases are released, deployments stop receiving work. Re-authentication is
refused while suspended.

### Rotate a device credential

Suspend → re-enroll with a fresh enroll token (old `dev_…` hash replaced).

### Drain a deployment

Client-side: set `tier2_max = 0` or `sharing_mode = "off"` → capacity report
drops to zero; router filters it out. In-flight requests finish.

### Backup

Dashboard → Settings → Backup (writes `pg_dump` to the server data dir), or:

```bash
docker compose exec postgres pg_dump -U loom loom > backup.sql
```

### Redis restart

Runtime state is ephemeral — the server's boot reconciliation clears stale
keys on restart; clients re-register and re-report capacity within one
heartbeat cycle. Queued/in-flight requests fail with `client_offline`/`overloaded`
— callers retry.

### Server restart

In-flight requests fail controlled (data stream closes → `upstream_error` /
`client_offline`); queued requests are lost (bodies are memory-only by
design). Clients reconnect with backoff automatically.

## Upgrades

### Server

Rolling: run a second replica, drain, swap — or accept the brief window
(clients reconnect, callers retry). DB migrations run at startup and are
forward-only.

### Client (loom-client)

Updater (enabled via config) checks `<server_url>/api/client/releases`:

1. Download new binary to `{data_dir}/updates/` next to the binary.
2. Verify SHA-256 checksum from the manifest.
3. Atomic rename → restart (systemd `Restart=always` picks the new binary).
4. On startup failure the previous binary is kept in `.backup` and restored.

Manual: replace `/usr/local/bin/loom-client`, `systemctl restart loom-client`.

## Protocol compatibility

`Hello` carries `protocol_version` + `protocol_version_min`. The server
rejects out-of-range clients (`protocol_version_unsupported`) — upgrade the
client. Breaking changes require a major version bump and a migration note
in release notes.

## Failure playbook

| Symptom | Check |
|---------|-------|
| Client offline | `journalctl -u loom-client`, server URL/TLS, credential status |
| Requests 503 `no deployment available` | deployments online? capabilities? tier2_free? |
| Requests 429 `overloaded` | queue depth (Scheduler page), class caps |
| `rate_limited` | API key limits; raise or add key |
| Leases stuck | `lease_sweep` runs each heartbeat cycle; fencing mismatches log `stale_fencing` |
