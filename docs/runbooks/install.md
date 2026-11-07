# Install & Enrollment Runbook

## Server (Docker Compose)

```bash
cd deploy
cp .env.example .env       # set POSTGRES_PASSWORD, LOOM_DOMAIN
docker compose --profile full up -d
```

- Dashboard: `https://<domain>/` (first visit → admin setup wizard)
- OpenAI API: `https://<domain>/v1`
- gRPC agent port: `50051` (configure TLS for production)

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
