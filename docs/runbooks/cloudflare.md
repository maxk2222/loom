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
- `agent.loom.example.com` → `https://127.0.0.1:50053` (agent gRPC, edge-mTLS)
- `boot.loom.example.com` → `https://127.0.0.1:50053` (same listener —
  initial cert issuance must work *without* a client cert, so this
  hostname carries no mTLS requirement)

The gRPC origin is the **bootstrap listener** (`LOOM_GRPC_BOOTSTRAP_BIND`)
on `127.0.0.1`: TLS server-auth without a client-cert requirement, and it
serves the *full* agent service — enrollment, CSR signing, sessions —
reachable only via the tunnel, never off-host.

> [!IMPORTANT]
> **`cloudflared` releases cannot speak h2c to the origin.** Prior-knowledge
> h2c for cleartext `http://` origins was merged upstream (commit `d4fbb16`,
> 2026-07) but is not yet in a released binary — an `http://` service with
> *Use HTTP/2 to origin* still dials **HTTP/1.1** and gRPC origins answer
> `502`. Until a release ships it, the working origin is **TLS**: point
> hostnames at `https://…:50053` with *No TLS Verify* + *Use HTTP/2 to
> origin* (h2 via ALPN). Once h2c lands, `http://…:50054`
> (`LOOM_GRPC_PLAIN_BIND`, loopback only) is the cleartext equivalent.

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
    service: https://127.0.0.1:50053
    originRequest: { noTLSVerify: true, http2Origin: true }
  - hostname: boot.loom.example.com
    service: https://127.0.0.1:50053
    originRequest: { noTLSVerify: true, http2Origin: true }
  - hostname: loom.example.com
    service: http://127.0.0.1:8080
  - service: http_status:404
```

### Server env

```bash
LOOM_GRPC_BOOTSTRAP_BIND=127.0.0.1:50053   # tunnel-facing agent listener (TLS, no client cert)
LOOM_TRUST_PROXY=1                        # honour CF-Connecting-IP / X-Forwarded-*
# Strict mTLS listener may stay up for direct LAN reachability — or be omitted.
LOOM_GRPC_BIND=127.0.0.1:50052
# Once released cloudflared supports h2c, a loopback LOOM_GRPC_PLAIN_BIND
# can serve as the origin instead of 50053.
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

## Option D — WebSocket agent transport (recommended through tunnels)

> [!IMPORTANT]
> **Bidirectional gRPC does not survive a cloudflared `--protocol http2`
> tunnel.** The http2 tunnel protocol buffers the entire request body
> until the client ends its stream — a control stream that never ends
> (by design) therefore never delivers its `Hello` to the origin. Measured
> behaviour: the server's `HelloAck` arrived exactly when the client's
> request stream closed. Unary calls and closed streams work fine, which
> is why this is easy to misdiagnose. QUIC (`--protocol quic`/`auto`)
> streams bodies properly, but tunnel QUIC needs outbound UDP 7844 —
> blocked on many networks.
>
> The durable answer is LOOM's **WebSocket transport**: the HTTP gateway
> (`:8080`) bridges the exact same control/data protocol over binary WS
> frames — `/api/agent/ws/control`, `/api/agent/ws/data` and the unary
> `/api/agent/config`. WS upgrades proxy live through any tunnel.

Client side this is just another endpoint URL scheme:

```toml
[connector]
endpoints = [
  { url = "https://server.lan:50052", pki = true },     # LAN mTLS — preferred
  { url = "https://server.ts.net:50052", pki = true },  # Tailscale
  { url = "wss://loom.example.com", pki = true },       # public fallback — WS
]
```

- `ws://`/`wss://` selects the WebSocket transport; `http(s)://` stays
  tonic gRPC. Endpoint order, failover probes and endpoint-hints merging
  are unchanged.
- A port-less `https://` endpoint automatically gains a `wss://` shadow
  at lowest priority (an edge-proxied host can never finish a buffered
  gRPC Hello), so `https://loom.example.com` alone still converges.
- `wss` uses the normal trust model: system roots + optional pinned
  `ca_cert_path`. `pki = true` on a WS endpoint means the CSR rides
  inside `Hello` — the server issues a cert that the client later
  presents when it migrates to a LAN/Tailscale mTLS endpoint.
- Ingress: the public hostname points at the **HTTP gateway**
  (`http://server:8080`), not a gRPC listener. WebSocket support must be
  enabled in the zone (it is by default).
- Set `cloudflared --protocol auto` (compose default): prefers QUIC
  where UDP egress exists, falls back to http2 elsewhere — the WS path
  works under both.

## Option C — multi-endpoint, credential-only public fallback

The simplest public path: no edge-mTLS, no bootstrap hostname. The
client keeps LAN/Tailscale mTLS endpoints preferred and falls back to
the tunnel when off-network. Identity is the device credential in
`Hello` — TLS terminates at the Cloudflare edge.

> [!WARNING]
> Under `--protocol http2` tunnels this option degrades to "connects,
> never finishes Hello" for the same buffering reason documented above —
> prefer `wss://` URLs (Option D) unless the tunnel can run QUIC.

```toml
# client config.toml
server_url = ""                       # unused when endpoints are set

[connector]
tls = "require"
endpoints = [
  { url = "https://server.lan:50052", pki = true },                  # LAN (mTLS + cert bootstrap)
  { url = "https://server.ts.net:50052", pki = true },               # Tailscale
  { url = "https://grpc.loom.example.com", pki = false },            # public fallback — credential only
]
```

- Endpoints are tried **in order** — earlier entries always win.
- On a fallback the client probes the preferred endpoints every 45 s
  and migrates back as soon as one answers.
- `pki = false` skips cert bootstrap/CSR on that endpoint (the client
  can still send a CSR in Hello when a hinted `pki` endpoint later
  becomes reachable — e.g. the LAN listener).
- `server_url` still works as the implicit single pki endpoint —
  existing configs are unchanged.

Server side, point a *second* tunnel public hostname at the bootstrap
listener — `grpc.loom.example.com` → `https://server:50053` with
*No TLS Verify* + *Use HTTP/2 to origin* (the listener serves the full
agent service, enrollment included). Enable *Network → gRPC* in
Cloudflare. No `agent.*`/`boot.*` split, no API Shield needed.

```bash
LOOM_GRPC_BOOTSTRAP_BIND=0.0.0.0:50053  # tunnel-facing — TLS, no client-cert requirement
```

Trade-offs vs. Option B: no client-cert check at the edge — anyone who
reaches the hostname may attempt Hello auth (still credential-gated,
rate-limitable). Prompt payloads also transit the CF edge in TLS —
fine as a fallback path; prefer LAN/Tailscale when available.

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
- **gRPC through the tunnel**: only works when cloudflared runs QUIC
  (`--protocol auto`/`quic` + outbound UDP 7844). Under the http2
  fallback the request body is buffered until stream end, so a
  bidirectional control stream never reaches the origin — use `wss://`
  endpoints (Option D) instead. Reconnect/backoff is unchanged on both
  transports. On released cloudflared the gRPC origin must be TLS
  (`https://` service + http2Origin + noTLSVerify) — cleartext `http://`
  origins are still dialed HTTP/1.1 until prior-knowledge h2c ships.
- **Real client IP**: with `LOOM_TRUST_PROXY=1`, audit logs and
  rate-limit buckets see the visitor IP, not the tunnel.
- **Bootstrap listener exposure**: `LOOM_GRPC_BOOTSTRAP_BIND` accepts
  any TLS client — app-layer credentials still gate everything (one-time
  `enroll_` token or `dev_` credential; CSRs are bound to the
  authenticated client id). Keep it compose-network/loopback; publishing
  it on a host interface or tunnel is exactly the intended public
  enrollment path, but then rate-limiting sits at the edge.
- **Plaintext agent listener**: bind `LOOM_GRPC_PLAIN_BIND` to loopback
  ONLY. On a public interface it accepts any client with a credential —
  no cert check. It is for the future h2c-capable cloudflared path or
  trusted local terminators, not for released cloudflared.

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
