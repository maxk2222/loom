<div align="center">

# 🔶 LOOM

### LLM Orchestration & Operations Mesh

**Turn idle workstation GPUs into a shared, centrally-managed inference mesh —**
**with hard guarantees that local workloads always come first.**

[![CI](https://github.com/maxk2222/loom/actions/workflows/ci.yml/badge.svg)](https://github.com/maxk2222/loom/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust)](https://www.rust-lang.org)
[![TypeScript](https://img.shields.io/badge/typescript-5.x-blue?logo=typescript)](https://www.typescriptlang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)
[![Protocol](https://img.shields.io/badge/protocol-loom.agent.v1-purple)](docs/protocol.md)

</div>

---

## ⚡ Quick Overview

Research labs are full of powerful workstations that sit idle most of the time.
**LOOM** pools that scattered capacity into one OpenAI-compatible inference
endpoint — without taking anything away from the people using the machines:

```text
curl https://loom.example.com/v1/chat/completions -H "Authorization: Bearer sk-…"
    ↓
central server (routing, queueing, auth, billing-grade metering)
    ↓
your request lands on whichever workstation has a free tier-2 slot —
the moment its owner needs the GPU, the offered capacity collapses to zero
```

| | |
|---|---|
| 🏠 **Local-first** | Workstation workloads are *always* prioritized — shared capacity is opportunistic spare, never guaranteed |
| 🔀 **Smart routing** | 5 strategies, capability-aware filtering, fallback chains, per-request routing traces |
| 🛡️ **Fence-protected** | Fencing tokens + atomic Redis Lua leases kill split-brain double-execution |
| 📊 **Full observability** | Dark dashboard, live SSE, TTFT p50/p99, per-project usage, audit trail |
| 🔌 **Real OpenAI API** | Chat · Completions · Embeddings · streaming · tools · vision |
| 🧩 **Any backend** | vLLM, Ollama, or any OpenAI-compatible server — capabilities auto-probed |

---


---

## 🖼️ Screenshots

<div align="center">
<img src="docs/screenshots/login.png" width="31%" alt="Login"/>
<img src="docs/screenshots/overview.png" width="31%" alt="Dashboard overview"/>
<img src="docs/screenshots/client.png" width="31%" alt="Client UI"/>
<br/>
<sub>Login · Admin dashboard · Workstation client UI — all dark-only, one codebase, two build variants</sub>
</div>

## 🏗️ Architecture

```mermaid
flowchart LR
    subgraph Callers["API Callers"]
        A1["OpenAI SDKs"]
        A2["LangChain / LlamaIndex"]
        A3["Custom tools"]
    end

    subgraph Server["🖥️ loom-server — central control plane"]
        GW["OpenAI Gateway<br/><code>/v1/*</code>"]
        AUTH["AuthN/Z<br/>API keys · scopes · limits"]
        REG["Model Registry<br/>virtual models"]
        RTR["Router<br/>5 strategies + caps filter"]
        Q["Tier-2 Scheduler<br/>priority queues · DRR fairness"]
        ADM["Admin REST +<br/>Dashboard"]
        GW --> AUTH --> RTR
        RTR --> Q
        REG --> RTR
    end

    subgraph Data["Runtime state"]
        PG[("PostgreSQL<br/>identity · config ·<br/>history · audit")]
        RD[("Redis<br/>queues · leases ·<br/>fencing · rate limits")]
    end

    subgraph WS1["💻 Workstation — loom-client"]
        CL1["Local admission<br/><b>owner always wins</b>"]
        BE1["vLLM / Ollama /<br/>OpenAI-compatible"]
        CL1 --- BE1
    end

    subgraph WS2["💻 Workstation — loom-client"]
        CL2["Local admission"]
        BE2["vLLM / Ollama"]
        CL2 --- BE2
    end

    A1 & A2 & A3 -->|HTTPS| GW
    Q -.->|"AssignRequest<br/>(fencing token)"| CL1
    Q -.->|"AssignRequest"| CL2
    CL1 & CL2 -->|"control stream<br/>hello · heartbeat · capacity"| Server
    CL1 & CL2 -->|"RequestData stream<br/>payload ⇄ response chunks"| Q
    Q --> RD
    AUTH --> RD
    ADM --> PG
    REG --> PG
    AUTH --> PG
```

**Key design invariant:** the server *never* talks to inference backends.
It only ever talks to clients — over a client-initiated, TLS-secured gRPC
channel — and the client's local admission control has the final word on
every single request.

<details>
<summary><b>📡 The channel split (click to expand)</b></summary>

```mermaid
sequenceDiagram
    participant C as loom-client
    participant S as loom-server
    participant B as Backend (vLLM)

    C->>S: Control stream: Hello (credential, inventory)
    S-->>C: HelloAck (session_id, client_id, device_cred)
    loop every 10s
        C->>S: Heartbeat + CapacityReport (tier2_free)
    end
    S-->>C: ConfigPush (desired state, versioned)

    Note over S: caller submits /v1/chat/completions<br/>→ route → queue → lease acquired
    S-->>C: AssignRequest (metadata only,<br/>request_id + lease + fencing)
    C->>S: AssignAck.accepted (local admission OK)
    C->>S: RequestData stream: head (fencing)
    S-->>C: request_json frames (payload, ≤4 MiB)
    C->>B: POST /v1/chat/completions
    B-->>C: SSE chunks
    C->>S: response_chunk frames (streamed)
    C->>S: end + Control: RequestEnd (usage, ttft)
```
</details>

---

## 🚀 Quickstart

### Server (Docker Compose — 2 minutes)

```bash
git clone https://github.com/maxk2222/loom.git && cd loom
cd deploy && cp .env.example .env      # set POSTGRES_PASSWORD, LOOM_DOMAIN
docker compose --profile full up -d
```

Open `https://localhost/` → the setup wizard creates your first admin.

### Workstation (one command)

In the dashboard: **Clients → Issue enroll token**, then on the workstation:

```bash
curl -fsSL https://loom.example.com/install-client.sh | \
  sudo bash -s -- --server https://loom.example.com:9443 --token enroll_…
```

The client registers, reports its backends, and appears online within seconds.

### First request

```bash
curl -N https://loom.example.com/v1/chat/completions \
  -H "Authorization: Bearer sk-your-project-key" \
  -H 'content-type: application/json' \
  -d '{
    "model": "vm-llama",
    "messages": [{"role": "user", "content": "Summarize the Higgs mechanism."}],
    "stream": true
  }'
```

<details>
<summary><b>Build from source instead</b></summary>

```bash
# Server
docker compose -f deploy/docker-compose.yml up -d postgres redis
export LOOM_DATABASE_URL=postgres://loom:loom@localhost:5432/loom
export LOOM_REDIS_URL=redis://localhost:6379/0
cargo run -p loom-server

# Dashboard dev server → http://localhost:5173
cd frontend && npm install && npm run dev

# Client
cargo run -p loom-client
```
</details>

---

## ✨ Feature Deep-Dive

### 🧠 Virtual models & capability-aware routing

Operators define **virtual models** (`vm-llama`) that map to one or more
deployments across the fleet. Members are filtered by:

- **api type** — chat / completions / embeddings
- **required capabilities** — tools, vision, streaming, json_mode
- **liveness** — client online, deployment enabled, `tier2_free > 0`
- **homogeneity** — optionally pin to identical model names

Then ranked by one of five strategies:

| Strategy | Behavior |
|----------|----------|
| `priority` | Lowest member priority wins |
| `least_in_flight` | Most free tier-2 slots first |
| `weighted` | Weighted shuffle by member weight |
| `round_robin` | Rotating fair share |
| `manual` | Pinned member first |

Every request carries a persisted **routing trace** — inspect *why* a request
went where it went.

### ⚖️ Fairness under contention

- Priority classes (project-level) with **strict class ordering**
- **FIFO inside each class** — no request bypasses an older one; requeues keep
  their original position
- Weighted DRR share between projects inside a class
- Global queue cap → clean `429 overloaded` instead of collapse
- At most **2 candidate retries** before stream start — none after (no
  double-generation ever)

### 🔒 Security by default

- Enrollment tokens are **one-time, short-lived** — exchanged for a rotatable
  device credential on first contact
- API keys SHA-256 hashed, scoped per endpoint family, with
  RPM/concurrency ceilings and project-level model grants
- Dashboard users get Argon2 + HttpOnly session cookies + roles
  (`admin` / `operator` / `viewer`)
- **Prompts and responses are never persisted** — not in Postgres, Redis,
  logs, or metrics
- gRPC fencing tokens make stale assignments harmless after reconnects

### 📟 The dashboard

Dark-only, zero build config needed at deploy time: live client status, queue
depths, virtual-model editor with member management, project/API-key/user
admin, usage charts, request history with routing traces, config history,
audit log — and **runtime branding** (name, logo, accent color) that applies
without a rebuild.

The **workstation client** embeds a minimal variant of the *same* React
codebase: connection state, backends with probed capabilities, live request
table, sharing controls.

---

## 🧬 How sharing actually works

```mermaid
flowchart TB
    subgraph Local["local priority path"]
        LQ["local request<br/>→ admit or 429<br/><b>never queued</b>"]
    end
    subgraph Slots["per-deployment slots"]
        M["max_concurrent = 4"]
        R["local_reserve = 1  (untouchable)"]
        T["tier2_max = 2  (offered)"]
    end
    subgraph Pressure["under local pressure"]
        P["tier2 offered → 0<br/>automatic, no admin action"]
    end
    LQ --> M
    T -.->|"sharing mode: auto"| P
```

- `auto` — tier-2 offer shrinks toward zero as local load rises
- `fixed` — constant offer
- `manual` / `private` — explicit control
- The central lease is a **reservation request**, not a grant — the client
  may reject any assignment, and the server simply tries the next candidate

---

## 📦 What's inside

```
crates/
  loom-core        shared types: OpenAI API, capabilities, errors, config
  loom-protocol    gRPC codegen (loom.agent.v1) + helpers
  loom-client      workstation daemon (local API :8391, connector, updater)
  loom-server      central server (gateway, scheduler, admin, gRPC)
  loom-cli         `loom` — enroll, status, diagnostics, admin ops
  loom-testkit     mocks + harness (mock backend, mock agent server)
proto/             agent.proto — the wire contract
migrations/        PostgreSQL schema (sqlx)
frontend/          React + Vite + Tailwind — dark-only, 2 build variants
deploy/            compose stack, Dockerfile, installer, systemd, Caddy
docs/              ADRs, protocol spec, security model, runbooks
```

---

## 📚 Documentation

| Doc | Contents |
|-----|----------|
| [protocol.md](docs/protocol.md) | Full wire spec — messages, fencing, bounds, reconnect |
| [security.md](docs/security.md) | Threat model, credentials, isolation invariants |
| [development.md](docs/development.md) | Dev loop, conventions, adding adapters |
| [runbooks/install.md](docs/runbooks/install.md) | Server + client install, enrollment, smoke test |
| [runbooks/operations.md](docs/runbooks/operations.md) | Monitoring, backups, upgrades, failure playbook |
| [adr/](docs/adr/) | Why gRPC-split channels, why Redis Lua, SMG evaluation, local-first |

---

## 🧪 Status & Roadmap

- [x] Control plane + data plane over client-initiated gRPC
- [x] Local-first admission + dynamic tier-2 sharing
- [x] OpenAI gateway: chat, completions, embeddings, streaming, tools, vision
- [x] Full admin dashboard + embedded client UI
- [x] Atomic leases, fencing, watchdogs, heartbeat detection
- [x] Installer, systemd, self-update pipeline
- [ ] mTLS for agent channel (TLS + device credential today)
- [ ] TPM metering (best-effort), project quotas
- [ ] Multi-replica scheduler coordination
- [ ] Signed release artifacts (minisign)

---

## 📄 License

MIT — see [LICENSE](LICENSE).

<div align="center">
<sub>LOOM — because your lab's GPUs are already there. They might as well be working.</sub>
</div>
