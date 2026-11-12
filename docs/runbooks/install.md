# Install & Enrollment Runbook

## Automated setup (recommended)

```bash
# Server — dirs + uid healing, CA + signed server cert, port autodetect,
# internal-only Postgres/Redis, admin + enroll token:
deploy/setup-server.sh --dir /data/loom \
    --admin-email you@example.com --enroll-token first-node

# Client — fetches CA via /api/tls/ca.crt, discovers Ollama/vLLM/LM Studio
# (incl. vLLM --api-key extraction from local containers), writes config,
# enrolls over mTLS bootstrap:
deploy/setup-client.sh --server https://<server>:50052 --token enroll_x \
    --dir /data/loom-client
```

Both scripts are idempotent. Notable automations: occupied ports are
re-mapped, TLS/UID/ownership problems self-heal via root entrypoints that
drop to uid 10001, and backend discovery builds deployments (vLLM gets a
dynamic `model_name = "auto"` deployment that follows whatever model is
loaded). `GET /api/tls/ca.crt` serves the public CA trust anchor — use it
for TOFU client setup (verify the fingerprint out-of-band on untrusted
networks).

The client keeps its state (client_id, device credential, issued certs) in
a named Docker volume. Your `--dir` only holds the generated config, CA and
compose file and is mounted read-only at `/etc/loom-seed` — the entrypoint
copies files into the volume whenever they are newer, so no host-uid
juggling is needed and atomic config updates always work. To change
backends/options on an installed client, re-run with `--reconfigure` — the
existing `client_id` and device credential are carried over, no new enroll
token is required.

## Server (Docker Compose, manual)

```bash
cd deploy
cp .env.example .env       # set POSTGRES_PASSWORD, LOOM_DOMAIN
docker compose --profile full up -d
```

- Dashboard: `https://<domain>/` (first visit → admin setup wizard)
- OpenAI API: `https://<domain>/v1`
- gRPC agent port: `50051` (see *TLS & mTLS* below for production)
- Behind Cloudflare Tunnel/WAF: see [cloudflare.md](cloudflare.md)

## TLS & mTLS (agent channel)

Recommended production layout — three listeners:

```bash
LOOM_GRPC_BIND=0.0.0.0:50052          # strict agent port (TLS + mTLS)
LOOM_GRPC_TLS_CERT=/etc/loom/server.crt
LOOM_GRPC_TLS_KEY=/etc/loom/server.key
LOOM_MTLS=1                            # require client certs signed by the CA
LOOM_GRPC_BOOTSTRAP_BIND=0.0.0.0:50053 # cert issuance (TLS, no client cert)
# optional: migrations, or the tunnel-facing origin for a public
# Cloudflare fallback — see runbooks/cloudflare.md; never map to a host port:
# LOOM_GRPC_PLAIN_BIND=0.0.0.0:50054
```

The server creates an internal CA at `$LOOM_DATA_DIR/tls/ca.{crt,key}` on
first boot (or reuses an existing CA directory — e.g. one generated with
openssl). Distribute `ca.crt` + an enroll token to each workstation; the
client mints its keypair, obtains a signed cert via the bootstrap port,
and renews automatically — no cert tooling on clients.

Client `config.toml`:

```toml
server_url = "https://loom.example.com:50052"

[connector]
tls = "require"                                   # require | auto | off
ca_cert_path = "/etc/loom-client/ca.crt"
# client_cert_path / client_key_path — leave unset for automatic PKI;
# set both to use externally-issued certs instead.
# bootstrap_url = "https://loom.example.com:50053" # default: :50053 on
#   standard agent ports (50051–50054); the server_url unchanged
#   otherwise (no port convention on :443 or custom ports).
```

The agent channel also speaks WebSocket: `wss://`/`ws://` endpoint URLs
open the control channel over the HTTP gateway's WS bridge instead of
gRPC — the transport to reach the server through buffered HTTP/2
proxies (Cloudflare tunnels without QUIC). See
[cloudflare.md](cloudflare.md#option-d--websocket-agent-transport-recommended-through-tunnels).

Verify issuance:

```bash
ls /etc/loom-client/tls/                 # client.{crt,key,meta} + server-ca.crt
openssl x509 -in /etc/loom-client/tls/client.crt -noout -subject -issuer
# subject=CN=loom-client-<client-id>  issuer=CN=LOOM Internal CA
```

First admin can also be created headless:

```bash
loom create-admin --database-url postgres://… --email admin@lab.org --password …
```

## Client (workstation)

1. In the dashboard → Clients → "Issue enroll token".
2. On the workstation, run the gateway-hosted installer — it downloads the
   newest matching release itself:

```bash
curl -fsSL https://loom.example.com/install-client.sh | \
  sudo bash -s -- --server https://loom.example.com:9443 --token enroll_…
```

   (The gateway prefills `--server` from the public `endpoint_hints` entry;
   fetched raw, `--server` is required. `LOOM_VERSION=vX.Y.Z` pins a
   version instead of latest.)

   **Docker instead of systemd/launchd:** pass `--docker` — no root needed
   beyond a reachable docker daemon:

```bash
curl -fsSL https://loom.example.com/install-client.sh | \
  bash -s -- --docker --token enroll_…
```

   This builds a local `loom-client:local` image from the signed release
   binary plus the gateway-served `docker-entrypoint-client.sh`, writes a
   compose project to `./loom-client-docker/` (env file with server URL,
   token, generated `LOOM_LOCAL_API_KEY`, optional `LOOM_BACKEND_*`/`LOOM_MODELS`
   bootstrap) and runs `docker compose up -d`. State lives in the named
   volume `loom-client-data`; the UI publishes on `127.0.0.1:8391`.
   For a full repo-checkout docker setup with backend auto-discovery use
   `deploy/setup-client.sh` instead.

What it does (native):

- installs `loom-client` + `loom` to `/usr/local/bin`
- pins the internal CA into `/etc/loom-client/ca.crt` — fetched from the
  gateway's `/api/tls/ca.crt` (same host on :8080 for agent ports,
  same-origin for public/`wss://` URLs) — and writes
  `connector.ca_cert_path`. Skipped silently when the server has no
  internal CA (mTLS disabled).
- writes `/etc/loom-client/config.toml` and `enroll.env` (mode 600)
- creates the `loom-client` user, installs + starts the systemd unit
  (on macOS: `~/Library/LaunchAgents/com.loom.client.plist`)

4. Configure backends in `/etc/loom-client/config.toml`, e.g.:

```toml
[[backends]]
name = "vllm-main"
type = "vllm"
url = "http://127.0.0.1:8000"

[[backends.deployments]]
name = "llama3-70b"
model_name = "meta-llama/Llama-3.3-70B-Instruct"
max_concurrent = 4
local_reserve = 1
tier2_max = 2
sharing_mode = "auto"
```

5. `systemctl restart loom-client` → the client registers, pushes inventory
   + capacity, appears online in the dashboard.

## Local verification

```bash
loom status                                   # local admin API
curl http://127.0.0.1:8391/healthz
curl http://127.0.0.1:8391/v1/models          # local priority API
```

The local UI/API binds loopback by default. For LAN access pass `--lan`
to `setup-client.sh` (docker), `LOOM_UI_BIND=0.0.0.0` in the generated
`.env` (install script docker method) or set `local_api.bind = "0.0.0.0:8391"`
(native) — always keep `local_api.api_key` configured; the setup flows
generate one automatically.

The configured key is managed from the client UI (Status → API key card):
reveal, copy, rotate (`POST /local/api/api-key {"rotate":true}`),
set a custom value, or clear it (API drops to open/loopback mode unless a
UI user exists). `GET /local/api/api-key` returns the plaintext key to
an authenticated caller; `DELETE` removes it. Cookie sessions survive a
rotation — bearer-authenticated tools must switch to the new key.

## Smoke test (central)

```bash
curl -N https://loom.example.com/v1/chat/completions \
  -H "Authorization: Bearer sk-…" \
  -H 'content-type: application/json' \
  -d '{"model":"vm-llama","messages":[{"role":"user","content":"hi"}],"stream":true}'
```
