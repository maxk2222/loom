#!/usr/bin/env bash
# LOOM client installer — downloads the newest release, installs loom-client
# + loom CLI, config and systemd/launchd unit.
# Usage (served by the LOOM gateway, server URL prefilled):
#   curl -fsSL https://<loom-gateway>/install-client.sh | sudo bash -s -- --token enroll_...
# Or with explicit values:
#   curl -fsSL https://<loom-gateway>/install-client.sh | sudo bash -s -- --server https://grpc.example.com --token enroll_...
# Docker instead of a native daemon (no root needed beyond docker access):
#   curl -fsSL https://<loom-gateway>/install-client.sh | bash -s -- --docker --token enroll_...
set -euo pipefail

# Substituted by the LOOM gateway when served from /install-client.sh; the
# placeholder makes --server mandatory when the script is fetched raw.
SERVER_URL="__LOOM_SERVER_URL__"
ENROLL_TOKEN=""
CLIENT_NAME="$(hostname -s 2>/dev/null || hostname)"
PREFIX="/usr/local"
METHOD="native"
WORK_DIR="${LOOM_DOCKER_DIR:-${PWD}/loom-client-docker}"
CONFIG_DIR="/etc/loom-client"
DATA_DIR="/var/lib/loom-client"
OS="$(uname -s)"
# Release source — override for forks/mirrors.
LOOM_REPO="${LOOM_REPO:-maxk2222/loom}"
LOOM_VERSION="${LOOM_VERSION:-latest}"

usage() {
  echo "Usage: install-client.sh [--docker] --server URL --token TOKEN [--name NAME] [--prefix DIR]"
  exit 1
}

SERVER_EXPLICIT=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --server) SERVER_URL="$2"; SERVER_EXPLICIT=1; shift 2 ;;
    --token) ENROLL_TOKEN="$2"; shift 2 ;;
    --name) CLIENT_NAME="$2"; shift 2 ;;
    --prefix) PREFIX="$2"; shift 2 ;;
    --docker) METHOD="docker"; shift ;;
    *) usage ;;
  esac
done
case "$SERVER_URL" in
  __LOOM_SERVER_URL__|"") usage ;;
esac

# Native install needs root (systemd + system dirs); the docker method only
# needs a reachable docker daemon.
if [[ "$METHOD" != "docker" && "$EUID" -ne 0 && "$OS" == "Linux" ]]; then
  echo "Please run as root (systemd + ${CONFIG_DIR})." >&2
  exit 1
fi

# Staging dir for binaries downloaded by this script (kept out of the caller's
# ./bin on purpose — we may be running as root in someone's $HOME).
STAGE_DIR=""
trap '[[ -n "$STAGE_DIR" ]] && rm -rf "$STAGE_DIR"' EXIT

lookup_bin() {
  local bin="$1"
  # ./bin first: a binary the user placed there is explicit intent (e.g. a
  # newer release on re-run) and must beat the already-installed PATH copy.
  if [[ -x "./bin/$bin" ]]; then
    echo "./bin/$bin"
  elif command -v "$bin" >/dev/null 2>&1; then
    command -v "$bin"
  elif [[ -n "$STAGE_DIR" && -x "${STAGE_DIR}/${bin}" ]]; then
    echo "${STAGE_DIR}/${bin}"
  else
    return 1
  fi
}

find_bin() {
  local bin="$1"
  lookup_bin "$bin" && return 0
  echo "!! $bin not found — place release binaries under ./bin/ or PATH" >&2
  return 1
}

# Optional binaries (e.g. the loom CLI before it shipped in releases): install
# when present, skip with a notice otherwise.
find_bin_opt() {
  lookup_bin "$1"
}

# --- newest-release download -------------------------------------------------

release_arch() {
  case "${OS}-$(uname -m)" in
    Linux-x86_64)            echo linux-x86_64 ;;
    Linux-aarch64|Linux-arm64) echo linux-aarch64 ;;
    Darwin-arm64)            echo macos-aarch64 ;;
    *) return 1 ;;
  esac
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

latest_tag() {
  # API first; fall back to the /releases/latest redirect target.
  local tag
  tag="$(curl -fsSL "https://api.github.com/repos/${LOOM_REPO}/releases/latest" 2>/dev/null \
        | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)"
  if [[ -z "$tag" ]]; then
    tag="$(curl -fsIL -o /dev/null -w '%{url_effective}' "https://github.com/${LOOM_REPO}/releases/latest" 2>/dev/null | sed 's#.*/##')"
  fi
  printf '%s' "$tag"
}

# Download loom-client (+ loom when the release ships it) into $STAGE_DIR so the
# find_bin path picks them up. sha256 is verified when published.
# RELEASE_ARCH_OVERRIDE forces the artifact arch (docker always needs linux/*).
download_release() {
  local arch ver base f want got
  arch="${RELEASE_ARCH_OVERRIDE:-$(release_arch)}" || { echo "!! no release for platform ${OS}-$(uname -m)" >&2; return 1; }
  ver="$LOOM_VERSION"
  [[ "$ver" == latest ]] && ver="$(latest_tag)"
  [[ -n "$ver" ]] || { echo "!! could not resolve latest release tag" >&2; return 1; }
  ver="${ver#v}"
  RESOLVED_VERSION="$ver"   # reused by docker_install's raw-file fallback
  base="https://github.com/${LOOM_REPO}/releases/download/v${ver}"
  STAGE_DIR="$(mktemp -d)"
  for b in loom-client loom; do
    f="${b}-${ver}-${arch}"
    if curl -fsSL -o "${STAGE_DIR}/${f}" "${base}/${f}" 2>/dev/null; then
      if curl -fsSL -o "${STAGE_DIR}/${f}.sha256" "${base}/${f}.sha256" 2>/dev/null; then
        want="$(tr -d '[:space:]' < "${STAGE_DIR}/${f}.sha256")"
        got="$(sha256_of "${STAGE_DIR}/${f}")"
        [[ "$want" == "$got" ]] || { echo "!! sha256 mismatch for ${f}" >&2; return 1; }
      fi
      install -m 0755 "${STAGE_DIR}/${f}" "${STAGE_DIR}/${b}"
      echo ">> downloaded ${f}"
    elif [[ "$b" == loom ]]; then
      echo ">> loom CLI not in release — skipping (optional)"
    else
      echo "!! download failed: ${base}/${f}" >&2
      return 1
    fi
  done
}

# Gateway candidates derived from --server:
#   https://host:5005{1..4} → same host on the gateway port convention :8080
#   wss://pub.example.com  → https://pub.example.com (the public tunnel route
#                           terminates on the gateway)
gw_candidates() {
  local url="$1" scheme rest host port
  scheme="${url%%://*}"
  rest="${url#*://}"
  host="${rest%%[:/]*}"
  port=""
  case "$rest" in
    *:*) port="${rest#*:}"; port="${port%%/*}" ;;
  esac
  case "$scheme" in
    wss|grpcs) scheme="https" ;;
    ws|grpc)   scheme="http" ;;
  esac
  case "$port" in
    50051|50052|50053|50054) echo "${scheme}://${host}:8080" ;;
    *) echo "${scheme}://${host}${port:+:$port}" ;;
  esac
}

# --- docker method -------------------------------------------------------------
# `--docker` builds a local image from the signed release binary plus the
# gateway-served container entrypoint, writes a compose project into
# $WORK_DIR and starts it. No registry required; client state lives in the
# named volume `loom-client-data`, ./seed carries the pinned CA into
# /etc/loom-seed for the entrypoint.
docker_install() {
  command -v docker >/dev/null 2>&1 || { echo "!! docker not found in PATH" >&2; exit 1; }
  docker compose version >/dev/null 2>&1 || { echo "!! docker compose plugin missing" >&2; exit 1; }
  case "$(uname -m)" in
    x86_64)        RELEASE_ARCH_OVERRIDE=linux-x86_64 ;;
    aarch64|arm64) RELEASE_ARCH_OVERRIDE=linux-aarch64 ;;
    *) echo "!! no linux release binary for $(uname -m)" >&2; exit 1 ;;
  esac

  mkdir -p "$WORK_DIR/seed"
  echo ">> Building docker client project in $WORK_DIR"

  # Build context: the linux release binary + the entrypoint served by the
  # same gateway that served this script.
  download_release
  install -m 0755 "${STAGE_DIR}/loom-client" "$WORK_DIR/loom-client"
  local fetched=0 base kflag
  for base in $(gw_candidates "$SERVER_URL"); do
    kflag=""
    case "$base" in *:8080*) kflag="-k" ;; esac
    if curl -fsSL $kflag -o "$WORK_DIR/docker-entrypoint-client.sh" "$base/docker-entrypoint-client.sh" 2>/dev/null \
       && grep -q 'loom-client' "$WORK_DIR/docker-entrypoint-client.sh" 2>/dev/null; then
      fetched=1
      break
    fi
    rm -f "$WORK_DIR/docker-entrypoint-client.sh"
  done
  if [[ "$fetched" == "0" ]]; then
    # Gateways older than this route → tag-pinned GitHub raw (matches the
    # release binary just downloaded; RESOLVED_VERSION set by download_release).
    local ver2="${RESOLVED_VERSION:-}"
    if [[ -z "$ver2" && "$LOOM_VERSION" != latest ]]; then ver2="${LOOM_VERSION#v}"; fi
    if [[ -n "$ver2" ]] \
       && curl -fsSL -o "$WORK_DIR/docker-entrypoint-client.sh" \
          "https://raw.githubusercontent.com/${LOOM_REPO}/v${ver2}/deploy/docker-entrypoint-client.sh" 2>/dev/null \
       && grep -q 'loom-client' "$WORK_DIR/docker-entrypoint-client.sh" 2>/dev/null; then
      fetched=1
    else
      rm -f "$WORK_DIR/docker-entrypoint-client.sh"
    fi
  fi
  if [[ "$fetched" == "0" ]]; then
    local self_dir
    self_dir="$(cd "$(dirname "$0")" && pwd)"
    if [[ -f "${self_dir}/docker-entrypoint-client.sh" ]]; then
      cp "${self_dir}/docker-entrypoint-client.sh" "$WORK_DIR/"
    else
      echo "!! could not fetch docker-entrypoint-client.sh (gateway + GitHub)" >&2
      exit 1
    fi
  fi
  chmod +x "$WORK_DIR/docker-entrypoint-client.sh"

  # Pinned internal CA → seed mount → /etc/loom-seed/ca.crt. Enables LAN /
  # Tailscale mTLS endpoints learned via endpoint hints after enrollment.
  for base in $(gw_candidates "$SERVER_URL"); do
    kflag=""
    case "$base" in *:8080*) kflag="-k" ;; esac
    if curl -fsSL $kflag -o "$WORK_DIR/seed/ca.crt.tmp" "$base/api/tls/ca.crt" 2>/dev/null \
       && grep -q "BEGIN CERTIFICATE" "$WORK_DIR/seed/ca.crt.tmp" 2>/dev/null; then
      mv "$WORK_DIR/seed/ca.crt.tmp" "$WORK_DIR/seed/ca.crt"
      echo ">> Pinned internal CA -> seed/ca.crt"
      break
    fi
    rm -f "$WORK_DIR/seed/ca.crt.tmp"
  done
  rmdir "$WORK_DIR/seed" 2>/dev/null || true   # drop empty seed dir when no CA

  cat > "$WORK_DIR/Dockerfile" <<'DOCKERFILE'
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 -m loom \
    && mkdir -p /etc/loom-client && chown loom:loom /etc/loom-client
COPY loom-client /usr/local/bin/loom-client
COPY docker-entrypoint-client.sh /usr/local/bin/docker-entrypoint-client.sh
RUN chmod +x /usr/local/bin/loom-client /usr/local/bin/docker-entrypoint-client.sh \
    && date +%s > /etc/loom-build-id
# Starts as root: the entrypoint ownership-heals the state dir then drops
# to uid 10001 via runuser.
USER root
ENV LOOM_CONFIG=/etc/loom-client/config.toml \
    LOOM_LOCAL_API_BIND=0.0.0.0:8391
VOLUME /etc/loom-client
EXPOSE 8391
HEALTHCHECK --interval=15s --timeout=3s --retries=5 \
    CMD curl -fsS http://localhost:8391/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/docker-entrypoint-client.sh"]
DOCKERFILE

  cat > "$WORK_DIR/compose.yml" <<'COMPOSE'
name: loom-client
services:
  loom-client:
    build: .
    image: loom-client:local
    container_name: loom-client
    restart: unless-stopped
    environment:
      LOOM_SERVER_URL: ${LOOM_SERVER_URL:?set LOOM_SERVER_URL in .env}
      LOOM_ENROLL_TOKEN: ${LOOM_ENROLL_TOKEN:-}
      LOOM_CLIENT_NAME: ${LOOM_CLIENT_NAME:-docker-node}
      LOOM_SHARING: ${LOOM_SHARING:-true}
      LOOM_LOCAL_API_KEY: ${LOOM_LOCAL_API_KEY:-}
      LOOM_PRESSURE_CPU: ${LOOM_PRESSURE_CPU:-0}
      LOOM_PRESSURE_RAM: ${LOOM_PRESSURE_RAM:-0}
      LOOM_UNLOAD_MODELS: ${LOOM_UNLOAD_MODELS:-false}
      LOOM_BACKEND_TYPE: ${LOOM_BACKEND_TYPE:-}
      LOOM_BACKEND_URL: ${LOOM_BACKEND_URL:-}
      LOOM_BACKEND_KEY: ${LOOM_BACKEND_KEY:-}
      LOOM_MODELS: ${LOOM_MODELS:-}
    extra_hosts:
      # Lets LOOM_BACKEND_URL=http://host.docker.internal:… reach services on
      # the Docker host (native on Docker Desktop, explicit on Linux).
      - "host.docker.internal:host-gateway"
    volumes:
      # Named volume keeps client_id + device credential + issued certs safe.
      - loom-client-data:/etc/loom-client
      - ./seed:/etc/loom-seed:ro
    ports:
      # Local dashboard + priority API — loopback-only by default. Set
      # LOOM_UI_BIND=0.0.0.0 in .env to reach it from the LAN.
      - "${LOOM_UI_BIND:-127.0.0.1}:${LOOM_UI_PORT:-8391}:8391"
volumes:
  loom-client-data:
COMPOSE

  # Fresh bearer key for the local API — the UI port is loopback-only by
  # default, but the key keeps the API protected when it gets exposed.
  local lkey=""
  if command -v openssl >/dev/null 2>&1; then
    lkey="lk-$(openssl rand -hex 16)"
  elif [[ -f /proc/sys/kernel/random/uuid ]]; then
    lkey="lk-$(tr -d '-' < /proc/sys/kernel/random/uuid)"
  fi

  # Existing state volume? Then config.toml (incl. the LIVE api key) persists
  # and the env vars below only matter for a fresh first start — flag that so
  # a re-run doesn't hand out a key that isn't actually in force.
  local have_state=0
  docker volume inspect loom-client_loom-client-data >/dev/null 2>&1 && have_state=1

  cat > "$WORK_DIR/.env" <<EOF
LOOM_SERVER_URL=${SERVER_URL}
LOOM_ENROLL_TOKEN=${ENROLL_TOKEN}
LOOM_CLIENT_NAME=${CLIENT_NAME}
LOOM_LOCAL_API_KEY=${lkey}
# Optional one-backend bootstrap, applied on the container's FIRST start
# (afterwards manage backends via the client UI):
# LOOM_BACKEND_TYPE=ollama
# LOOM_BACKEND_URL=http://host.docker.internal:11434
# LOOM_MODELS=model-a,model-b
EOF
  chmod 600 "$WORK_DIR/.env"
  # Under sudo the project lands root-owned in the caller's $PWD — hand it
  # back so later `docker compose` runs can read .env as the real user.
  if [[ -n "${SUDO_UID:-}" && -n "${SUDO_GID:-}" ]]; then
    chown -R "${SUDO_UID}:${SUDO_GID}" "$WORK_DIR" 2>/dev/null || true
  fi

  (cd "$WORK_DIR" && docker compose up -d --build)

  echo ""
  echo "Done. Client UI:   http://127.0.0.1:8391"
  if [[ "$have_state" == "1" ]]; then
    echo "NOTE: existing client state kept — the configured local API key is unchanged"
    echo "      (read/rotate it in the client UI → Status → API key)."
  elif [[ -n "$lkey" ]]; then
    echo "Local API key:   $lkey"
  fi
  echo "Project dir:     $WORK_DIR   (docker compose logs -f | down)"
}

if [[ "$METHOD" == "docker" ]]; then
  docker_install
  exit 0
fi

# Fetch the newest release when the client binary isn't already available.
# (docker method fetches its own linux/* artifact inside docker_install.)
if [[ "$METHOD" != "docker" ]] && ! find_bin_opt loom-client >/dev/null 2>&1; then
  echo ">> loom-client not found locally — downloading latest release"
  download_release
fi

BIN_DIR="${PREFIX}/bin"
if [[ "$OS" == "Linux" ]]; then
  echo ">> Creating ${CONFIG_DIR}, ${DATA_DIR}"
  mkdir -p "$CONFIG_DIR" "$DATA_DIR" "${DATA_DIR}/bin"
  chmod 750 "$CONFIG_DIR"
  id -u loom-client &>/dev/null || useradd -r -d "$DATA_DIR" -s /usr/sbin/nologin loom-client

  # The daemon binary lives in the data dir on purpose: the systemd unit uses
  # ProtectSystem=strict, which makes ${PREFIX} read-only — a binary under
  # ReadWritePaths is the only location the self-updater may swap in place.
  # /usr/local/bin/loom-client stays a convenience symlink for manual runs.
  client_bin="$(find_bin loom-client)"
  # -ef guard: re-runs resolve the /usr/local/bin symlink back to the data-dir
  # binary — installing a file onto itself errors out.
  if [[ "$client_bin" -ef "${DATA_DIR}/bin/loom-client" ]]; then
    echo ">> loom-client already installed (same binary)"
  else
    install -m 0755 "$client_bin" "${DATA_DIR}/bin/loom-client"
  fi
  chown -R loom-client:loom-client "$DATA_DIR"
  if loom_bin="$(find_bin_opt loom)" && [[ -n "$loom_bin" ]]; then
    [[ "$loom_bin" -ef "${BIN_DIR}/loom" ]] || install -m 0755 "$loom_bin" "${BIN_DIR}/loom"
  else
    echo ">> loom CLI not found — skipping (optional)"
  fi
  ln -sf "${DATA_DIR}/bin/loom-client" "${BIN_DIR}/loom-client"
else
  CONFIG_DIR="${HOME}/Library/Application Support/loom-client"
  mkdir -p "$CONFIG_DIR"
  client_bin="$(find_bin loom-client)"
  if [[ "$client_bin" -ef "${BIN_DIR}/loom-client" ]]; then
    echo ">> loom-client already installed (same binary)"
  else
    install -m 0755 "$client_bin" "${BIN_DIR}/loom-client"
  fi
  if loom_bin="$(find_bin_opt loom)" && [[ -n "$loom_bin" ]]; then
    [[ "$loom_bin" -ef "${BIN_DIR}/loom" ]] || install -m 0755 "$loom_bin" "${BIN_DIR}/loom"
  else
    echo ">> loom CLI not found — skipping (optional)"
  fi
fi

# --- internal CA pin --------------------------------------------------------
# The gateway serves the internal CA at /api/tls/ca.crt (a public trust
# anchor — possession grants no access; device credentials and issued certs
# still gate every session). Pinning it up-front lets the client verify
# LAN/Tailscale mTLS endpoints immediately: the cert bootstrap listener and
# later endpoint-hints failovers learned after enrollment.
CA_FILE="${CONFIG_DIR}/ca.crt"
if [[ ! -f "$CA_FILE" ]]; then
  for base in $(gw_candidates "$SERVER_URL"); do
    # Internal gateway ports answer with the internal CA's own chain — not yet
    # trusted, so that fetch is TOFU like the bootstrap listener. Public hosts
    # verify normally.
    kflag=""
    case "$base" in *:8080*) kflag="-k" ;; esac
    if curl -fsSL $kflag -o "$CA_FILE.tmp" "$base/api/tls/ca.crt" 2>/dev/null \
       && grep -q "BEGIN CERTIFICATE" "$CA_FILE.tmp" 2>/dev/null; then
      mv "$CA_FILE.tmp" "$CA_FILE"
      echo ">> Pinned internal CA from $base -> $CA_FILE"
      break
    fi
    rm -f "$CA_FILE.tmp"
  done
fi

CFG="${CONFIG_DIR}/config.toml"
if [[ ! -f "$CFG" ]]; then
  echo ">> Writing $CFG"
  cat > "$CFG" <<EOF
name = "${CLIENT_NAME}"
server_url = "${SERVER_URL}"
sharing_enabled = true

[local_api]
bind = "127.0.0.1:8391"

[updater]
enabled = true
EOF
  if [[ -f "$CA_FILE" ]]; then
    cat >> "$CFG" <<EOF

[connector]
ca_cert_path = "${CA_FILE}"
EOF
  fi
  [[ "$OS" == "Linux" ]] && chmod 640 "$CFG" && chown root:loom-client "$CFG"
else
  echo ">> Keeping existing config at $CFG"
  # An explicitly passed --server is migration intent — retarget the
  # existing config's server_url (e.g. dead gRPC host → wss:// bridge).
  # The gateway-baked default leaves the file alone.
  if [[ "$SERVER_EXPLICIT" == "1" ]] && grep -q '^server_url' "$CFG"; then
    sed -i "s|^server_url = .*|server_url = \"${SERVER_URL}\"|" "$CFG"
    echo ">> Updated server_url in existing config -> ${SERVER_URL}"
  fi
fi

# Enroll: pass token via env for first connect (device credential is stored after enrollment).
if [[ -n "$ENROLL_TOKEN" ]]; then
  echo ">> Writing enrollment token to ${CONFIG_DIR}/enroll.env"
  printf 'LOOM_ENROLL_TOKEN=%s\n' "$ENROLL_TOKEN" > "${CONFIG_DIR}/enroll.env"
  chmod 600 "${CONFIG_DIR}/enroll.env"
  [[ "$OS" == "Linux" ]] && chown root:loom-client "${CONFIG_DIR}/enroll.env"
fi

# The daemon writes state next to the config — the mTLS cert store, enrollment
# credentials, setup-wizard users, config-version. It must own the directory
# (systemd ReadWritePaths below grants the write permission).
[[ "$OS" == "Linux" ]] && chown -R loom-client:loom-client "$CONFIG_DIR"

if [[ "$OS" == "Linux" ]] && command -v systemctl >/dev/null 2>&1; then
  echo ">> Installing systemd unit"
  cat > /etc/systemd/system/loom-client.service <<'UNIT'
[Unit]
Description=LOOM Client (local inference node)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=loom-client
Group=loom-client
EnvironmentFile=-/etc/loom-client/enroll.env
Environment=LOOM_CONFIG=/etc/loom-client/config.toml
ExecStart=/var/lib/loom-client/bin/loom-client
Restart=always
RestartSec=3
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/loom-client /etc/loom-client

[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable --now loom-client
  echo ">> loom-client enabled and started"
elif [[ "$OS" == "Darwin" ]]; then
  # Install as a proper .app bundle: menu-bar agent (LSUIElement → no Dock
  # icon, system-tray icon only) auto-started via launchd LaunchAgent.
  APP_DIR="${HOME}/Applications/LOOM Client.app"
  echo ">> Installing ${APP_DIR}"
  mkdir -p "${APP_DIR}/Contents/MacOS" "${APP_DIR}/Contents/Resources"
  install -m 0755 "${BIN_DIR}/loom-client" "${APP_DIR}/Contents/MacOS/loom-client"
  cat > "${APP_DIR}/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>LOOM Client</string>
  <key>CFBundleDisplayName</key><string>LOOM Client</string>
  <key>CFBundleIdentifier</key><string>com.loom.client</string>
  <key>CFBundleExecutable</key><string>loom-client</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>0.2.0</string>
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
</dict></plist>
PLIST
  SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
  if [[ -f "${SCRIPT_DIR}/macos/AppIcon.icns" ]]; then
    install -m 0644 "${SCRIPT_DIR}/macos/AppIcon.icns" "${APP_DIR}/Contents/Resources/AppIcon.icns"
  fi
  touch "${APP_DIR}" # refresh LaunchServices timestamp

  # Enable the tray icon unless the operator opted out.
  if ! grep -q '^\[tray\]' "$CFG"; then
    printf '\n[tray]\nenabled = true\n' >> "$CFG"
  fi

  PLIST="${HOME}/Library/LaunchAgents/com.loom.client.plist"
  mkdir -p "$(dirname "$PLIST")"
  cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.loom.client</string>
  <key>ProgramArguments</key><array><string>${APP_DIR}/Contents/MacOS/loom-client</string></array>
  <key>EnvironmentVariables</key><dict>
    <key>LOOM_CONFIG</key><string>${CFG}</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>${CONFIG_DIR}/client.log</string>
  <key>StandardErrorPath</key><string>${CONFIG_DIR}/client.log</string>
</dict></plist>
PLIST
  launchctl unload "$PLIST" 2>/dev/null || true
  launchctl load "$PLIST"
  echo ">> LOOM Client.app installed — tray icon active, autostart via launchd"
fi

echo ""
echo "Done. Local API:  http://127.0.0.1:8391"
echo "Status:          loom status"
echo "Logs (linux):    journalctl -u loom-client -f"
