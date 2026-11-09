#!/usr/bin/env bash
# LOOM client — automated Docker setup.
#
#   deploy/setup-client.sh --server https://host:50052 --token enroll_x [options]
#
# - fetches the server CA (file, HTTP /api/tls/ca.crt, or local tls dir)
# - auto-discovers local inference backends (Ollama, vLLM, LM Studio)
#   incl. --api-key extraction from local vLLM containers
# - writes config.toml + a self-contained compose file with correct uid
# - enrolls over the mTLS bootstrap channel and verifies registration
#
# Options:
#   --server URL        agent endpoint, e.g. https://loom.local:50052  (required)
#   --token T           enroll token (required on first run)
#   --name N            client name          (default: hostname)
#   --dir DIR           state dir            (default: ./loom-client-state)
#   --src DIR           build context        (default: repo root)
#   --image IMG         prebuilt image instead of building from --src
#   --ca FILE           ca.crt path          (default: fetch from server HTTP)
#   --ca-url URL        explicit CA fetch URL
#   --server-http URL   server HTTP base     (default: http(s)://<host>:8080)
#   --ollama URL        add Ollama backend   (default: probe localhost:11434)
#   --vllm URL          add vLLM backend     (default: probe localhost:8003..8000)
#   --vllm-key K        vLLM API key (else: container inspect → prompt)
#   --lmstudio URL      add LM Studio backend(default: probe localhost:1234)
#   --models a,b        restrict discovered models to this list
#   --auto              give every backend a dynamic "auto" deployment too
#   --reconfigure       regenerate config for an existing install (keeps
#                       client_id + device credential — no new token needed)
#   --sharing           sharing on/off       (default: true)
#   --pressure-cpu N    tier-2 gate %        (default: 0 = off)
#   --pressure-ram N    tier-2 gate %        (default: 0 = off)
set -euo pipefail

SERVER=""; TOKEN="${LOOM_ENROLL_TOKEN:-}"; NAME="$(hostname)"
DIR="./loom-client-state"; SRC=""; IMAGE=""
CA_FILE=""; CA_URL=""; SERVER_HTTP=""
OLLAMA=""; VLLM=""; VLLM_KEY=""; LMSTUDIO=""
MODELS=""; ADD_AUTO=0; SHARING=true; PCPU=0; PRAM=0
PROBE_OLLAMA=1; PROBE_VLLM=1; PROBE_LMS=1; RECONFIGURE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --server) SERVER="$2"; shift 2;;
        --token) TOKEN="$2"; shift 2;;
        --name) NAME="$2"; shift 2;;
        --dir) DIR="$2"; shift 2;;
        --src) SRC="$2"; shift 2;;
        --image) IMAGE="$2"; shift 2;;
        --ca) CA_FILE="$2"; shift 2;;
        --ca-url) CA_URL="$2"; shift 2;;
        --server-http) SERVER_HTTP="$2"; shift 2;;
        --ollama) OLLAMA="$2"; PROBE_OLLAMA=0; shift 2;;
        --vllm) VLLM="$2"; PROBE_VLLM=0; shift 2;;
        --vllm-key) VLLM_KEY="$2"; shift 2;;
        --lmstudio) LMSTUDIO="$2"; PROBE_LMS=0; shift 2;;
        --models) MODELS="$2"; shift 2;;
        --auto) ADD_AUTO=1; shift;;
        --reconfigure) RECONFIGURE=1; shift;;
        --sharing) SHARING="$2"; shift 2;;
        --pressure-cpu) PCPU="$2"; shift 2;;
        --pressure-ram) PRAM="$2"; shift 2;;
        *) echo "unknown option: $1" >&2; exit 2;;
    esac
done

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()   { printf '    \033[1;32m✓\033[0m %s\n' "$*"; }
warn() { printf '    \033[1;33m!\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[ -n "$SERVER" ] || die "--server is required (e.g. https://loom.local:50052)"
[ -z "$SRC" ] && SRC="$(cd "$(dirname "$0")/.." && pwd)"
for c in docker curl; do command -v "$c" >/dev/null || die "$c is required"; done
docker compose version >/dev/null 2>&1 || die "docker compose plugin required"

# Normalize server URL → https
case "$SERVER" in http://*|https://*) ;; *) SERVER="https://$SERVER";; esac
SHOST=$(printf '%s' "$SERVER" | sed -E 's|https?://([^:/]+).*|\1|')
SPORT=$(printf '%s' "$SERVER" | grep -oE ':[0-9]+' | tail -1 | tr -d ':' || true)
[ -z "$SPORT" ] && SPORT=443
[ -z "$SERVER_HTTP" ] && SERVER_HTTP="http://$SHOST:8080"

# --- free port picker (multiple clients on one host must not collide) -------
port_free() {
    ! { ss -tlnH "sport = :$1" 2>/dev/null || netstat -tln 2>/dev/null; } | grep -q ":$1 "
}
pick_port() { local p="$1" i=0; while [ $i -lt 51 ]; do port_free "$p" && { echo "$p"; return; }; p=$((p+1)); i=$((i+1)); done; echo "$1"; }
LAPI_PORT=$(pick_port 8391)
# Inside the container the API always binds 8391; on host-networking (linux)
# that IS the host port, on bridge (darwin) the port map translates.
CFG_LAPI_PORT=8391
uname -s | grep -qi darwin || CFG_LAPI_PORT="$LAPI_PORT"

# --- CA trust anchor ----------------------------------------------------------
mkdir -p "$DIR"; chmod 700 "$DIR"
if [ -z "$CA_FILE" ]; then
    for cand in "$CA_URL" "$SERVER_HTTP/api/tls/ca.crt" \
                /data/loom/tls/ca.crt /opt/loom/tls/ca.crt; do
        [ -z "$cand" ] && continue
        case "$cand" in
            http*) curl -fsS --max-time 8 "$cand" -o "$DIR/ca.crt" 2>/dev/null && CA_FILE="$DIR/ca.crt" && break;;
            *)     [ -s "$cand" ] && cp "$cand" "$DIR/ca.crt" 2>/dev/null && CA_FILE="$DIR/ca.crt" && break;;
        esac
    done
else cp "$CA_FILE" "$DIR/ca.crt"; CA_FILE="$DIR/ca.crt"; fi
[ -n "$CA_FILE" ] && [ -s "$CA_FILE" ] || die "no ca.crt — pass --ca or run with the server reachable"
grep -q "BEGIN CERTIFICATE" "$CA_FILE" || die "$CA_FILE is not a PEM certificate"
ok "ca.crt ← $CA_FILE (TOFU — verify fingerprint out-of-band for untrusted networks)"

# --- backend discovery ----------------------------------------------------------
# No -f: we want the status code itself (000 on connect failure).
get() { curl -sS --max-time 4 -o /dev/null -w '%{http_code}' ${3:+-H "Authorization: Bearer $3"} "$1" 2>/dev/null || true; }
models_of() { # url authkey → newline list (openai /v1/models OR ollama /api/tags)
    local u="$1" k="$2" j
    j=$(curl -fsS --max-time 4 ${k:+-H "Authorization: Bearer $k"} "$u/v1/models" 2>/dev/null || true)
    [ -n "$j" ] || j=$(curl -fsS --max-time 4 "$u/api/tags" 2>/dev/null || true)
    printf '%s' "$j" | grep -oE '"id"\s*:\s*"[^"]+"|"name"\s*:\s*"[^"]+"' | cut -d'"' -f4 | sort -u
}
container_key() { # port → --api-key/-api_key value from a local container's args/env
    local port="$1" cid
    cid=$(docker ps --format '{{.ID}} {{.Ports}}' | grep ":$port->" | cut -d' ' -f1 | head -1) || return 1
    [ -z "$cid" ] && return 1
    docker inspect "$cid" --format '{{json .Config.Cmd}}' \
        | grep -oE '"--api[-_]key",[^]]*' | grep -oE '"[^"]+"' | sed -n '2p' | tr -d '"'
}

BACKENDS=""          # TOML text accumulated
add_backend() { # name type endpoint key
    local n="$1" t="$2" ep="$3" key="$4" id
    id=$(cat /proc/sys/kernel/random/uuid 2>/dev/null || uuidgen)
    BACKENDS="$BACKENDS
[[backends]]
id = \"$id\"
name = \"$n\"
backend_type = \"$t\"
endpoint = \"$ep\"
api_key = \"$key\"
enabled = true
"
}
add_dep() { # name model autoload
    local n="$1" m="$2" al="${3:-true}" did
    did=$(cat /proc/sys/kernel/random/uuid 2>/dev/null || uuidgen)
    BACKENDS="$BACKENDS
[[backends.deployments]]
id = \"$did\"
name = \"$n\"
model_name = \"$m\"
max_concurrent = 4
local_reserve = 1
tier2_max = 4
sharing_mode = \"shared\"
auto_load = $al
enabled = true
"
}
in_model_list() { [ -z "$MODELS" ] || printf '%s' ",$MODELS," | grep -q ",$1,"; }

# Ollama
[ "$PROBE_OLLAMA" = "1" ] && [ -z "$OLLAMA" ] && \
    [ "$(get http://localhost:11434/api/tags)" = "200" ] && OLLAMA="http://localhost:11434"
if [ -n "$OLLAMA" ]; then
    add_backend "ollama" "ollama" "$OLLAMA" ""
    while IFS= read -r m; do
        [ -n "$m" ] && in_model_list "$m" && add_dep "$m" "$m"
    done < <(models_of "$OLLAMA" "")
    [ "$ADD_AUTO" = "1" ] && add_dep "ollama-auto" "auto"
    ok "ollama ← $OLLAMA"
fi

# vLLM (probe common ports; --vllm overrides)
if [ "$PROBE_VLLM" = "1" ] && [ -z "$VLLM" ]; then
    for p in 8003 8000 8001 8002; do
        code=$(get "http://localhost:$p/v1/models")
        [ "$code" = "200" ] || [ "$code" = "401" ] && { VLLM="http://localhost:$p"; break; }
    done
fi
if [ -n "$VLLM" ]; then
    code=$(get "$VLLM/v1/models" "" "$VLLM_KEY")
    if [ "$code" = "401" ] && [ -z "$VLLM_KEY" ]; then
        port="${VLLM##*:}"
        VLLM_KEY=$(container_key "$port" || true)
        [ -n "$VLLM_KEY" ] && code=$(get "$VLLM/v1/models" "" "$VLLM_KEY")
        [ "$code" = "200" ] && ok "vllm api key auto-extracted from container args"
    fi
    if [ "$code" != "200" ]; then
        warn "vLLM at $VLLM needs an API key (got HTTP $code) — rerun with --vllm-key; skipping backend"
        VLLM=""
    fi
fi
if [ -n "$VLLM" ]; then
    add_backend "vllm" "vllm" "$VLLM" "$VLLM_KEY"
    # vLLM's single resident model changes on restart — the dynamic "auto"
    # binding follows whatever is served, no config edits needed.
    add_dep "vllm-auto" "auto" false
    ok "vllm ← $VLLM (dynamic auto-binding)"
fi

# LM Studio
[ "$PROBE_LMS" = "1" ] && [ -z "$LMSTUDIO" ] && \
    { [ "$(get http://localhost:1234/v1/models)" = "200" ] || [ "$(get http://localhost:1234/api/v1/models)" = "200" ]; } && \
    LMSTUDIO="http://localhost:1234"
if [ -n "$LMSTUDIO" ]; then
    add_backend "lmstudio" "generic_openai" "$LMSTUDIO" ""
    while IFS= read -r m; do
        [ -n "$m" ] && in_model_list "$m" && add_dep "$m" "$m"
    done < <(models_of "$LMSTUDIO" "")
    [ "$ADD_AUTO" = "1" ] && add_dep "lmstudio-auto" "auto"
    ok "lmstudio ← $LMSTUDIO"
fi

[ -n "$BACKENDS" ] || warn "no backends discovered — add them later via the local UI (:8391)"

# --- config.toml ------------------------------------------------------------------
CFG="$DIR/config.toml"
ENROLLED=0; KEEP_ID=""; KEEP_CRED=""
if [ -s "$CFG" ] && grep -q '^client_id = "[0-9a-f]' "$CFG"; then
    ENROLLED=1
    KEEP_ID=$(grep -m1 '^client_id = "' "$CFG" || true)
    KEEP_CRED=$(grep -m1 '^device_credential = "' "$CFG" || true)
fi
if [ "$ENROLLED" = "1" ] && [ "$RECONFIGURE" = "0" ]; then
    ok "existing enrolled config reused (client_id present)"
    { [ "$PROBE_OLLAMA" = "0" ] || [ "$PROBE_VLLM" = "0" ] || [ "$PROBE_LMS" = "0" ] || [ "$ADD_AUTO" = "1" ] || [ -n "$MODELS" ]; } && \
        warn "backend options ignored on an existing install — pass --reconfigure to apply them"
else
    LKEY=$(openssl rand -hex 24)
    cat > "$CFG" <<EOF
name = "$NAME"
server_url = "$SERVER"
connect_enabled = true
sharing_enabled = $SHARING
$KEEP_ID
$KEEP_CRED
[local_api]
bind = "127.0.0.1:$CFG_LAPI_PORT"
api_key = "$LKEY"
serve_ui = true

[connector]
tls = "require"
ca_cert_path = "/etc/loom-client/ca.crt"

[pressure]
enabled = true
cpu_pct = $PCPU
ram_pct = $PRAM
unload_models = false
cooldown_secs = 30

[updater]
enabled = false

[tray]
enabled = false
$BACKENDS
EOF
    ok "config.toml written"
fi
chmod 600 "$CFG" || true
# NOTE: $DIR stays host-owned — it is mounted read-only as /etc/loom-seed and
# copied into the state volume by the entrypoint, so no uid juggling needed.

# --- compose ----------------------------------------------------------------------
BUILD_BLOCK="    build:
      context: $SRC
      dockerfile: deploy/Dockerfile.client
    image: loom-client:local"
[ -n "$IMAGE" ] && BUILD_BLOCK="    image: $IMAGE"

NET="    network_mode: host"
uname -s | grep -qi darwin && NET="    extra_hosts:
      - \"host.docker.internal:host-gateway\"
    ports:
      - \"127.0.0.1:$LAPI_PORT:8391\""

# Project name derives from the install dir so multiple client installs on
# one host never adopt/replace each other's containers.
PROJ="loom-client-$(basename "$DIR" | tr -cd 'a-zA-Z0-9_-')"

cat > "$DIR/docker-compose.yml" <<EOF
# Generated by deploy/setup-client.sh — safe to re-run.
name: $PROJ

services:
  client:
$BUILD_BLOCK
    restart: unless-stopped
$NET
    environment:
      LOOM_ENROLL_TOKEN: \${LOOM_ENROLL_TOKEN:-}
      LOOM_CONFIG: /etc/loom-client/config.toml
      RUST_LOG: "info,loom_client=debug"
    volumes:
      # host seed (read-only): config.toml + ca.crt land in the state volume
      - $DIR:/etc/loom-seed:ro
      - loom-client-state:/etc/loom-client

volumes:
  loom-client-state:
EOF

say "starting client"
TOKEN_ENV=""
[ -n "$TOKEN" ] && TOKEN_ENV="LOOM_ENROLL_TOKEN=$TOKEN"
env $TOKEN_ENV docker compose -f "$DIR/docker-compose.yml" up -d --build >/dev/null

# --- verify registration -----------------------------------------------------------
say "waiting for enrollment/registration"
LOGS() { docker compose -f "$DIR/docker-compose.yml" logs client 2>&1; }
REG=""
for i in $(seq 1 30); do
    if LOGS | grep -q "registered with server"; then REG=1; break; fi
    sleep 2
done
if [ -n "$REG" ]; then
    CID=$(LOGS | grep -oE 'client_id.{0,50}' | tail -1 | grep -oE '[0-9a-f-]{36}' || true)
    ok "registered with server (client_id $CID)"
else
    LOGS | tail -15 | sed 's/^/    /'
    die "client did not register — see logs above"
fi

cat <<EOS

$(say "LOOM client up")
  local api : http://127.0.0.1:$LAPI_PORT/ (key in $DIR/config.toml)
  state     : $DIR
  compose   : $DIR/docker-compose.yml  (project $PROJ)
EOS
