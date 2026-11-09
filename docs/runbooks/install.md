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
# optional during migration only:
LOOM_GRPC_PLAIN_BIND=0.0.0.0:50051     # legacy plaintext — remove once done
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
# bootstrap_url = "https://loom.example.com:50053" # default: server port+1
```

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
2. Copy the release bundle (binaries + `deploy/install-client.sh`) to the
   workstation.
3. Run:

```bash
sudo ./install-client.sh --server https://loom.example.com:9443 --token enroll_…
```

What it does:

- installs `loom-client` + `loom` to `/usr/local/bin`
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

## Smoke test (central)

```bash
curl -N https://loom.example.com/v1/chat/completions \
  -H "Authorization: Bearer sk-…" \
  -H 'content-type: application/json' \
  -d '{"model":"vm-llama","messages":[{"role":"user","content":"hi"}],"stream":true}'
```
