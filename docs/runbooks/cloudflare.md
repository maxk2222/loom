# Runbook — LOOM behind Cloudflare (Tunnel + WAF)

How to expose LOOM's HTTP surface and agent channel through Cloudflare
without breaking authentication. Read this alongside
[install.md](install.md) and [../security.md](../security.md).

## Why the agent channel needs special handling

LOOM's mTLS check happens **inside the TLS handshake** on the gRPC
listener (`:50052`). Any TLS-terminating proxy — Cloudflare, an LB, a
reverse proxy — breaks that handshake chain: the edge sees the client
cert, the origin never does. Two consequences:

- The strict mTLS port cannot be proxied by a TLS terminator. The client
  certificate either goes to Cloudflare's edge (API Shield mTLS) or the
  endpoint must stay direct.
- `cloudflared` cannot present a client cert to the origin either — the
  origin listener it targets must therefore trust the tunnel itself
  (loopback plaintext is fine; the tunnel is already encrypted).

## Topology overview

```
                        ┌────────────── Cloudflare edge ──────────────┐
                        │                                             │
  Dashboard/API users ──┤ loom.example.com      (public TLS, WAF)     │
                        │ agent.loom.example.com (mTLS via API Shield)│
                        │ boot.loom.example.com  (public TLS, no mTLS)│
                        └───────┬──────────────────────┬──────────────┘
                                │ cloudflared tunnel   │
                        ┌───────▼──────────────────────▼──────────────┐
   LOOM server          │ :8080 HTTP        :50054 gRPC h2c (loopback)│
                        │ :50052 mTLS direct (optional, LAN/hybrid)   │
                        │ :50053 bootstrap direct (optional)          │
                        └─────────────────────────────────────────────┘
```

- `loom.example.com` → `http://127.0.0.1:8080` (dashboard + `/v1` + `/api`)
- `agent.loom.example.com` → `http://127.0.0.1:50054` (agent gRPC, edge-mTLS)
- `boot.loom.example.com` → `http://127.0.0.1:50054` (same listener —
  initial cert issuance must work *without* a client cert, so this
  hostname carries no mTLS requirement)

The loopback gRPC listener is `LOOM_GRPC_PLAIN_BIND` bound to
`127.0.0.1` — reachable only via the tunnel, never off-host.

## Option A — hybrid (recommended when ports can be opened)

Keep the agent channel off Cloudflare entirely:

- `loom.example.com` (orange cloud) → dashboard + API through the tunnel.
- `agents.loom.example.com` **grey cloud** (DNS-only, direct IP) →
  `server_ip:50052` / `:50053` with the existing mTLS listeners. Nothing
  changes for clients.

Best of both: WAF + TLS in front of the public surface, unmodified
end-to-end mTLS on the control channel. Requires ports 50052/50053 open
to the workstations (they only need outbound).

## Option B — edge-mTLS through the tunnel

When agent ports cannot be exposed, enforce mTLS at the Cloudflare edge
instead of the origin:

1. **Upload the internal CA** to Cloudflare:
   `data/tls/ca.crt` → SSL/TLS → Client Certificates → *Upload CA
   certificate* (API Shield mTLS). This is the same CA that signs all
   workstation certs.
2. **Create an mTLS rule** on `agent.loom.example.com` requiring a valid
   client certificate. Cloudflare drops cert-less connections before they
   reach the tunnel.
3. **Do not** require mTLS on `boot.loom.example.com` — initial issuance
   happens there before a cert exists.
4. Origin sees HTTP/2 gRPC with the device credential in `Hello` — the
   app-layer credential check still authenticates the client.

Security posture: cert-gated at the edge + credential-verified at the
origin. What is lost vs. direct mTLS: the cert-CN↔device_id binding at
the origin (Cloudflare, not the server, checked the cert). Optionally
restore it with a Worker that injects the verified DN as a header:

```js
// Worker on agent.loom.example.com
export default {
  async fetch(request) {
    const dn = request.cf?.tlsClientAuth?.certSubjectDN || "";
    const h = new Headers(request.headers);
    h.set("x-loom-client-cert-dn", dn);
    return fetch(request, { headers: h });
  },
};
```

### cloudflared ingress (`~/.cloudflared/config.yml`)

```yaml
tunnel: <tunnel-id>
credentials-file: /etc/cloudflared/<tunnel-id>.json
ingress:
  - hostname: agent.loom.example.com
    service: http://127.0.0.1:50054
  - hostname: boot.loom.example.com
    service: http://127.0.0.1:50054
  - hostname: loom.example.com
    service: http://127.0.0.1:8080
  - service: http_status:404
```

### Server env

```bash
LOOM_GRPC_PLAIN_BIND=127.0.0.1:50054   # tunnel-facing agent+bootstrap listener
LOOM_TRUST_PROXY=1                    # honour CF-Connecting-IP / X-Forwarded-*
# Strict listeners may stay up for direct LAN reachability — or be omitted.
LOOM_GRPC_BIND=127.0.0.1:50052
LOOM_GRPC_BOOTSTRAP_BIND=127.0.0.1:50053
```

> `LOOM_TRUST_PROXY=1` tells the rate limiter, login throttle, audit log
> and `Secure`-cookie logic to use `CF-Connecting-IP`/`X-Forwarded-*`
> instead of the TCP peer. **Only set it when every request really passes
> the proxy** — on a directly reachable server it makes rate limits
> spoofable. cloudflared overwrites `CF-Connecting-IP` itself, so
> clients cannot inject it past the edge.

### Client config (per workstation)

```toml
server_url = "https://agent.loom.example.com"

[connector]
tls = "require"
bootstrap_url = "https://boot.loom.example.com"
# ca_cert_path may stay empty: the client trusts public CAs already.
# The issued internal-CA client cert is presented automatically when
# the edge asks for one (CertificateRequest).
```

Flow: no cert → Hello+CSR via `boot.*` → cert issued → all traffic via
`agent.*` (edge verifies the internal-CA cert). Renewal rides the same
path automatically.

## Caveats that matter in practice

- **100 s request timeout** (Free/Pro/Biz): long *non-streaming*
  completions die at the edge as `524`. Use `"stream": true` — streamed
  chunks keep the connection alive; idle streams are covered by the
  client's 20 s HTTP/2 keepalive.
- **WAF false positives**: LLM prompts routinely contain strings that
  resemble SQLi/XSS payloads. Add a WAF skip rule for `/v1/*` (and the
  agent hostnames) or tune specific managed rulesets.
- **Body limits**: 100 MB (Free) / 200 MB (Pro) per request — relevant
  only for large audio/video uploads.
- **gRPC through the tunnel**: cloudflared forwards HTTP/2; the
  bidirectional stream + 20 s keepalive survive it. Reconnect/backoff is
  unchanged.
- **Real client IP**: with `LOOM_TRUST_PROXY=1`, audit logs and
  rate-limit buckets see the visitor IP, not the tunnel.
- **Plaintext agent listener**: bind `LOOM_GRPC_PLAIN_BIND` to loopback
  ONLY. On a public interface it accepts any client with a credential —
  no cert check.

## Verifying the setup

```bash
# Dashboard through CF
curl -I https://loom.example.com/healthz

# Agent channel: without a cert the edge must reject before origin
curl -sv https://agent.loom.example.com --resolve 'agent.loom.example.com:443:<cf-ip>' 

# Bootstrap must accept TLS without a client cert
curl -sv https://boot.loom.example.com

# Client end-to-end
loom-client --config config.toml   # watch for "registered with server"
```

Audit-login entries in the dashboard should now show the real visitor
IP, and `loom_session` cookies carry `Secure` (X-Forwarded-Proto is
honoured under `LOOM_TRUST_PROXY`).
