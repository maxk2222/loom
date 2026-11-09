#!/usr/bin/env bash
# LOOM server — automated one-shot setup.
#
#   deploy/setup-server.sh [options]
#
# Creates a self-contained deployment directory with:
#   - data dirs owned for the container uid (10001)
#   - an internal CA + CA-signed server certificate (SANs auto-detected)
#   - a docker-compose stack: internal-only Postgres+Redis, mTLS agent
#     listener, cert-issuance bootstrap listener, HTTP gateway/dashboard
#   - auto-selected free ports when the defaults are occupied
#   - optional first admin + enroll token minting
#
# Idempotent: existing certs/env/compose are reused on re-run.
#
# Options:
#   --dir DIR            install dir            (default: ./loom-server-deploy)
#   --src DIR            build context          (default: repo root of this script)
#   --image IMG          use prebuilt image instead of building from --src
#   --hostname H         primary SAN name       (default: hostname -f)
#   --san NAME|IP        extra SAN, repeatable
#   --http-port P        preferred HTTP port    (default: 8080)
#   --grpc-port P        preferred agent port   (default: 50052)
#   --bootstrap-port P   preferred PKI port     (default: 50053)
#   --no-publish-db      (default: DB/Redis never published — informational)
#   --publish-db         expose Postgres/Redis on 127.0.0.1 random free ports
#   --admin-email E      create first admin (asks password if --admin-password unset)
#   --admin-password P   admin password (generated if unset and email given)
#   --enroll-token LABEL mint an enroll token after setup
#   --project NAME       compose project name   (default: loom)
set -euo pipefail

DIR="./loom-server-deploy"; SRC=""; IMAGE=""
HOSTNAME="$(hostname -f 2>/dev/null || hostname)"; EXTRA_SANS=()
HTTP_PORT=8080; GRPC_PORT=50052; BOOT_PORT=50053
PUBLISH_DB=0; ADMIN_EMAIL=""; ADMIN_PW=""; ENROLL_LABEL=""; PROJECT="loom"

while [ $# -gt 0 ]; do
    case "$1" in
        --dir) DIR="$2"; shift 2;;
        --src) SRC="$2"; shift 2;;
        --image) IMAGE="$2"; shift 2;;
        --hostname) HOSTNAME="$2"; shift 2;;
        --san) EXTRA_SANS+=("$2"); shift 2;;
        --http-port) HTTP_PORT="$2"; shift 2;;
        --grpc-port) GRPC_PORT="$2"; shift 2;;
        --bootstrap-port) BOOT_PORT="$2"; shift 2;;
        --publish-db) PUBLISH_DB=1; shift;;
        --no-publish-db) PUBLISH_DB=0; shift;;
        --admin-email) ADMIN_EMAIL="$2"; shift 2;;
        --admin-password) ADMIN_PW="$2"; shift 2;;
        --enroll-token) ENROLL_LABEL="$2"; shift 2;;
        --project) PROJECT="$2"; shift 2;;
        *) echo "unknown option: $1" >&2; exit 2;;
    esac
done

say()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()   { printf '    \033[1;32m✓\033[0m %s\n' "$*"; }
warn() { printf '    \033[1;33m!\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[ -z "$SRC" ] && SRC="$(cd "$(dirname "$0")/.." && pwd)"

# --- dependencies -----------------------------------------------------------
for c in docker openssl curl; do command -v "$c" >/dev/null || die "$c is required"; done
docker compose version >/dev/null 2>&1 || die "docker compose plugin is required"
ok "docker + compose + openssl + curl"

# --- clock sanity (TLS + apt both break on large skew) ------------------------
REMOTE_TS=$(curl -fsSI --max-time 8 https://github.com 2>/dev/null | grep -i '^date:' | cut -d' ' -f2- | xargs || true)
if [ -n "$REMOTE_TS" ]; then
    R=$(date -u -d "$REMOTE_TS" +%s 2>/dev/null || date -u -j -f "%a, %d %b %Y %T GMT" "$REMOTE_TS" +%s 2>/dev/null || echo 0)
    L=$(date -u +%s); D=$((L > R ? L - R : R - L))
    if [ "$D" -gt 120 ]; then
        warn "system clock is ${D}s off — fix NTP before building (apt + TLS break on skew)"
    else ok "clock sane (±${D}s)"; fi
else warn "could not verify clock against github.com"; fi

# --- free port picker ---------------------------------------------------------
port_free() {
    ! { ss -tlnH "sport = :$1" 2>/dev/null || netstat -tln 2>/dev/null; } | grep -q ":$1 "
}
pick_port() { # preferred → first free of +0..+50
    local p="$1" i=0
    while [ $i -lt 51 ]; do
        if port_free "$p"; then echo "$p"; return 0; fi
        p=$((p + 1)); i=$((i + 1))
    done
    die "no free port near $1"
}
HTTP_PORT=$(pick_port "$HTTP_PORT"); GRPC_PORT=$(pick_port "$GRPC_PORT"); BOOT_PORT=$(pick_port "$BOOT_PORT")
ok "ports: http=$HTTP_PORT grpc=$GRPC_PORT bootstrap=$BOOT_PORT"

# --- dirs + permissions -------------------------------------------------------
if ! mkdir -p "$DIR/tls" "$DIR/state" 2>/dev/null; then
    say "creating $DIR needs elevated permissions"
    sudo mkdir -p "$DIR/tls" "$DIR/state" && sudo chown -R "$(id -u):$(id -g)" "$DIR" \
        || die "could not create install dir $DIR"
fi
chmod 700 "$DIR"
# container uid 10001 (see Dockerfiles); bind-mounted dirs must be readable
chown -R 10001:10001 "$DIR/tls" "$DIR/state" 2>/dev/null || {
    warn "chown to 10001 failed (no root?) — entrypoint will heal at start"
}
ok "install dir $DIR"

# --- CA + server certificate ---------------------------------------------------
if [ ! -s "$DIR/tls/ca.crt" ] || [ ! -s "$DIR/tls/ca.key" ]; then
    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$DIR/tls/ca.key" 2>/dev/null
    openssl req -x509 -new -key "$DIR/tls/ca.key" -subj "/CN=LOOM Internal CA" -days 3650 \
        -out "$DIR/tls/ca.crt" \
        -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
    ok "internal CA created"
else ok "internal CA reused"; fi

SAN="DNS:$HOSTNAME,DNS:localhost,IP:127.0.0.1"
for ip in $(hostname -I 2>/dev/null); do SAN="$SAN,IP:$ip"; done
if command -v tailscale >/dev/null; then
    TS_IP=$(tailscale ip -4 2>/dev/null || true)
    [ -n "$TS_IP" ] && SAN="$SAN,IP:$TS_IP"
    # MagicDNS name (host.tailnet.ts.net) — remote clients dial this name.
    TS_DNS=$(tailscale status --json 2>/dev/null | grep -o '"DNSName": *"[^"]*"' \
        | head -1 | cut -d'"' -f4 | sed 's/\.$//' || true)
    [ -n "$TS_DNS" ] && SAN="$SAN,DNS:$TS_DNS"
fi
for s in ${EXTRA_SANS[@]+"${EXTRA_SANS[@]}"}; do
    case "$s" in *[!0-9.]*) SAN="$SAN,DNS:$s";; *) SAN="$SAN,IP:$s";; esac
done

cert_covers_sans() { # all wanted SANs present in the current cert?
    local have s rc=0
    have=$(openssl x509 -in "$1" -noout -ext subjectAltName 2>/dev/null \
        | sed 's/IP Address:/IP:/g' || true)
    [ -n "$have" ] || return 1
    for s in $(printf '%s' "$SAN" | tr ',' '\n'); do
        printf '%s' "$have" | grep -qF "$s" || { rc=1; break; }
    done
    return $rc
}

if [ ! -s "$DIR/tls/server.key" ]; then
    openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$DIR/tls/server.key" 2>/dev/null \
        || die "server key generation failed"
fi
if [ ! -s "$DIR/tls/server.crt" ] || ! cert_covers_sans "$DIR/tls/server.crt"; then
    [ -s "$DIR/tls/server.crt" ] && warn "SAN set changed — re-signing server cert"
    openssl req -new -key "$DIR/tls/server.key" -subj "/CN=loom-server" -out "$DIR/tls/server.csr"
    printf 'subjectAltName=%s\n' "$SAN" > "$DIR/tls/san.cnf"
    openssl x509 -req -in "$DIR/tls/server.csr" -CA "$DIR/tls/ca.crt" -CAkey "$DIR/tls/ca.key" \
        -CAcreateserial -days 730 -out "$DIR/tls/server.crt" -extfile "$DIR/tls/san.cnf" \
        || die "server cert signing failed"
    rm -f "$DIR/tls/server.csr"
    ok "server cert signed (SAN: $SAN)"
else ok "server cert reused (covers all SANs)"; fi
chmod 600 "$DIR/tls/ca.key" "$DIR/tls/server.key" 2>/dev/null || true
chown -R 10001:10001 "$DIR/tls" 2>/dev/null || true

# --- secrets (.env) -------------------------------------------------------------
if [ ! -s "$DIR/.env" ]; then
    PG_PW=$(openssl rand -hex 16)
    cat > "$DIR/.env" <<EOF
POSTGRES_USER=loom
POSTGRES_PASSWORD=$PG_PW
POSTGRES_DB=loom
EOF
    chmod 600 "$DIR/.env"; ok "generated .env"
else ok "reused .env"; fi
# shellcheck disable=SC1091
set -a; . "$DIR/.env"; set +a

# --- compose ---------------------------------------------------------------------
DB_PORTS=""
if [ "$PUBLISH_DB" = "1" ]; then
    PGP=$(pick_port 5432); RDP=$(pick_port 6379)
    DB_PORTS="    ports:
      - \"127.0.0.1:$PGP:5432\""
    REDIS_PORTS="    ports:
      - \"127.0.0.1:$RDP:6379\""
fi

BUILD_BLOCK="    build:
      context: $SRC
      dockerfile: deploy/Dockerfile.server
    image: loom-server:local"
[ -n "$IMAGE" ] && BUILD_BLOCK="    image: $IMAGE"

cat > "$DIR/docker-compose.yml" <<EOF
# Generated by deploy/setup-server.sh — re-running the script refreshes this
# file. Postgres + Redis are internal-only to the compose network (no host
# ports → no collisions with existing databases).
name: $PROJECT

services:
  postgres:
    image: postgres:17-alpine
    restart: unless-stopped
    environment:
      POSTGRES_USER: \${POSTGRES_USER}
      POSTGRES_PASSWORD: \${POSTGRES_PASSWORD}
      POSTGRES_DB: \${POSTGRES_DB}
    volumes:
      - pgdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U \$\${POSTGRES_USER} -d \$\${POSTGRES_DB}"]
      interval: 5s
      timeout: 3s
      retries: 12
${DB_PORTS:+$DB_PORTS}
  redis:
    image: redis:7-alpine
    restart: unless-stopped
    command: redis-server --maxmemory 512mb --maxmemory-policy noeviction --appendonly no --save ""
    volumes:
      - redisdata:/data
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 5s
      timeout: 3s
      retries: 12
${REDIS_PORTS:-}
  server:
$BUILD_BLOCK
    restart: unless-stopped
    depends_on:
      postgres:
        condition: service_healthy
      redis:
        condition: service_healthy
    environment:
      LOOM_DATABASE_URL: postgres://\${POSTGRES_USER}:\${POSTGRES_PASSWORD}@postgres:5432/\${POSTGRES_DB}
      LOOM_REDIS_URL: redis://redis:6379/0
      LOOM_REDIS_PREFIX: "loom:prod:"
      LOOM_HTTP_BIND: "0.0.0.0:8080"
      LOOM_GRPC_BIND: "0.0.0.0:50052"
      LOOM_GRPC_BOOTSTRAP_BIND: "0.0.0.0:50053"
      LOOM_GRPC_TLS_CERT: /data/tls/server.crt
      LOOM_GRPC_TLS_KEY: /data/tls/server.key
      LOOM_MTLS: "1"
      LOOM_DATA_DIR: /data
      LOOM_DASHBOARD_DIR: /var/lib/loom/dashboard
      RUST_LOG: "info,loom_server=debug"
    volumes:
      - serverdata:/data
      - ./tls:/data/tls
    ports:
      - "$HTTP_PORT:8080"
      - "$GRPC_PORT:50052"
      - "$BOOT_PORT:50053"

volumes:
  pgdata:
  redisdata:
  serverdata:
EOF
ok "compose written"

# --- up + wait ------------------------------------------------------------------
say "starting stack"
docker compose -f "$DIR/docker-compose.yml" --env-file "$DIR/.env" up -d --build >/dev/null
for i in $(seq 1 60); do
    [ "$(curl -fsS --max-time 2 "http://localhost:$HTTP_PORT/readyz" 2>/dev/null || true)" = "ready" ] && break
    sleep 2
done
[ "$(curl -fsS --max-time 5 "http://localhost:$HTTP_PORT/readyz" 2>/dev/null || true)" = "ready" ] \
    || die "server did not become ready — check: docker compose -f $DIR/docker-compose.yml logs server"
ok "server ready on :$HTTP_PORT"

# --- admin + enroll token ---------------------------------------------------------
SETUP=$(curl -fsS --max-time 5 "http://localhost:$HTTP_PORT/api/setup/status" 2>/dev/null | grep -o '"needs_setup":[a-z]*' | cut -d: -f2 || true)
COOKIE="$DIR/.cookie"
if [ "$SETUP" = "true" ]; then
    if [ -n "$ADMIN_EMAIL" ]; then
        [ -z "$ADMIN_PW" ] && ADMIN_PW=$(openssl rand -hex 12)
        curl -fsS -X POST "http://localhost:$HTTP_PORT/api/setup/admin" -H 'Content-Type: application/json' \
            -d "{\"email\":\"$ADMIN_EMAIL\",\"password\":\"$ADMIN_PW\",\"name\":\"$ADMIN_EMAIL\"}" >/dev/null
        ok "admin created: $ADMIN_EMAIL (password in $DIR/.admin-password)"
        printf '%s\n' "$ADMIN_PW" > "$DIR/.admin-password"; chmod 600 "$DIR/.admin-password"
    else
        warn "no admin yet — finish via dashboard or --admin-email"
    fi
fi
if [ -z "$ADMIN_PW" ] && [ -s "$DIR/.admin-password" ]; then ADMIN_PW=$(cat "$DIR/.admin-password"); fi
if [ -n "$ADMIN_EMAIL" ] && [ -n "$ADMIN_PW" ]; then
    curl -fsS -c "$COOKIE" -X POST "http://localhost:$HTTP_PORT/api/auth/login" -H 'Content-Type: application/json' \
        -d "{\"email\":\"$ADMIN_EMAIL\",\"password\":\"$ADMIN_PW\"}" >/dev/null 2>&1 || warn "admin login failed"
fi
if [ -n "$ENROLL_LABEL" ] && [ -s "$COOKIE" ]; then
    TOK=$(curl -fsS -b "$COOKIE" -X POST "http://localhost:$HTTP_PORT/api/enroll-tokens" \
        -H 'Content-Type: application/json' -d "{\"label\":\"$ENROLL_LABEL\",\"max_uses\":10,\"expires_hours\":72}" \
        2>/dev/null | grep -o '"token":"[^"]*"' | cut -d'"' -f4 || true)
    [ -n "$TOK" ] && { printf '%s\n' "$TOK" > "$DIR/.enroll-token"; chmod 600 "$DIR/.enroll-token"; ok "enroll token: $TOK"; }
fi
# Client updater manifest base — without it clients guess the HTTP endpoint
# from the gRPC port, which only works on defaults.
if [ -s "$COOKIE" ]; then
    curl -fsS -b "$COOKIE" -X PUT "http://localhost:$HTTP_PORT/api/settings/public_base" \
        -H 'Content-Type: application/json' -d "{\"value\":\"http://$HOSTNAME:$HTTP_PORT\"}" >/dev/null 2>&1 \
        || warn "could not set public_base — set it later via PUT /api/settings/public_base"
fi

cat <<EOS

$(say "LOOM server up")
  dashboard : http://$HOSTNAME:$HTTP_PORT/ui/
  openai api: http://$HOSTNAME:$HTTP_PORT/v1
  agent     : mTLS  $HOSTNAME:$GRPC_PORT
  bootstrap : $HOSTNAME:$BOOT_PORT
  ca.crt    : $DIR/tls/ca.crt  (also GET /api/tls/ca.crt)
$([ -n "$ENROLL_LABEL" ] && echo "  enroll    : $DIR/.enroll-token")
EOS
