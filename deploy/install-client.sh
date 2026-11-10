#!/usr/bin/env bash
# LOOM client installer — downloads the newest release, installs loom-client
# + loom CLI, config and systemd/launchd unit.
# Usage (served by the LOOM gateway, server URL prefilled):
#   curl -fsSL https://<loom-gateway>/install-client.sh | sudo bash -s -- --token enroll_...
# Or with explicit values:
#   curl -fsSL https://<loom-gateway>/install-client.sh | sudo bash -s -- --server https://grpc.example.com --token enroll_...
set -euo pipefail

# Substituted by the LOOM gateway when served from /install-client.sh; the
# placeholder makes --server mandatory when the script is fetched raw.
SERVER_URL="__LOOM_SERVER_URL__"
ENROLL_TOKEN=""
CLIENT_NAME="$(hostname -s 2>/dev/null || hostname)"
PREFIX="/usr/local"
CONFIG_DIR="/etc/loom-client"
DATA_DIR="/var/lib/loom-client"
OS="$(uname -s)"
# Release source — override for forks/mirrors.
LOOM_REPO="${LOOM_REPO:-maxk2222/loom}"
LOOM_VERSION="${LOOM_VERSION:-latest}"

usage() {
  echo "Usage: install-client.sh --server URL --token TOKEN [--name NAME] [--prefix DIR]"
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --server) SERVER_URL="$2"; shift 2 ;;
    --token) ENROLL_TOKEN="$2"; shift 2 ;;
    --name) CLIENT_NAME="$2"; shift 2 ;;
    --prefix) PREFIX="$2"; shift 2 ;;
    *) usage ;;
  esac
done
case "$SERVER_URL" in
  __LOOM_SERVER_URL__|"") usage ;;
esac

if [[ "$EUID" -ne 0 ]] && [[ "$OS" == "Linux" ]]; then
  echo "Please run as root (systemd + ${CONFIG_DIR})." >&2
  exit 1
fi

find_bin() {
  local bin="$1"
  if command -v "$bin" >/dev/null 2>&1; then
    command -v "$bin"
  elif [[ -x "./bin/$bin" ]]; then
    echo "./bin/$bin"
  else
    echo "!! $bin not found — place release binaries under ./bin/ or PATH" >&2
    exit 1
  fi
}

# Optional binaries (e.g. the loom CLI before it shipped in releases): install
# when present, skip with a notice otherwise.
find_bin_opt() {
  local bin="$1"
  if command -v "$bin" >/dev/null 2>&1; then
    command -v "$bin"
  elif [[ -x "./bin/$bin" ]]; then
    echo "./bin/$bin"
  fi
}

# --- newest-release download -------------------------------------------------

release_arch() {
  case "${OS}-$(uname -m)" in
    Linux-x86_64)            echo linux-x86_64 ;;
    Linux-aarch64|Linux-arm64) echo linux-aarch64 ;;
    Darwin-arm64)            echo macos-aarch64 ;;
    Darwin-x86_64)           echo macos-x86_64 ;;
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

# Download loom-client (+ loom when the release ships it) into ./bin so the
# regular find_bin path picks them up. sha256 is verified when published.
download_release() {
  local arch ver base tmp f want got
  arch="$(release_arch)" || { echo "!! no release for platform ${OS}-$(uname -m)" >&2; return 1; }
  ver="$LOOM_VERSION"
  [[ "$ver" == latest ]] && ver="$(latest_tag)"
  [[ -n "$ver" ]] || { echo "!! could not resolve latest release tag" >&2; return 1; }
  ver="${ver#v}"
  base="https://github.com/${LOOM_REPO}/releases/download/v${ver}"
  tmp="$(mktemp -d)"
  mkdir -p ./bin
  for b in loom-client loom; do
    f="${b}-${ver}-${arch}"
    if curl -fsSL -o "${tmp}/${f}" "${base}/${f}" 2>/dev/null; then
      if curl -fsSL -o "${tmp}/${f}.sha256" "${base}/${f}.sha256" 2>/dev/null; then
        want="$(tr -d '[:space:]' < "${tmp}/${f}.sha256")"
        got="$(sha256_of "${tmp}/${f}")"
        [[ "$want" == "$got" ]] || { echo "!! sha256 mismatch for ${f}" >&2; return 1; }
      fi
      install -m 0755 "${tmp}/${f}" "./bin/${b}"
      echo ">> downloaded ${f}"
    elif [[ "$b" == loom ]]; then
      echo ">> loom CLI not in release — skipping (optional)"
    else
      echo "!! download failed: ${base}/${f}" >&2
      return 1
    fi
  done
  rm -rf "$tmp"
}

# Fetch the newest release when the client binary isn't already available.
if ! find_bin_opt loom-client >/dev/null 2>&1; then
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
  install -m 0755 "$(find_bin loom-client)" "${DATA_DIR}/bin/loom-client"
  chown -R loom-client:loom-client "$DATA_DIR"
  if loom_bin="$(find_bin_opt loom)" && [[ -n "$loom_bin" ]]; then
    install -m 0755 "$loom_bin" "${BIN_DIR}/loom"
  else
    echo ">> loom CLI not found — skipping (optional)"
  fi
  ln -sf "${DATA_DIR}/bin/loom-client" "${BIN_DIR}/loom-client"
else
  CONFIG_DIR="${HOME}/Library/Application Support/loom-client"
  mkdir -p "$CONFIG_DIR"
  install -m 0755 "$(find_bin loom-client)" "${BIN_DIR}/loom-client"
  if loom_bin="$(find_bin_opt loom)" && [[ -n "$loom_bin" ]]; then
    install -m 0755 "$loom_bin" "${BIN_DIR}/loom"
  else
    echo ">> loom CLI not found — skipping (optional)"
  fi
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
  [[ "$OS" == "Linux" ]] && chmod 640 "$CFG" && chown root:loom-client "$CFG"
else
  echo ">> Keeping existing config at $CFG"
fi

# Enroll: pass token via env for first connect (device credential is stored after enrollment).
if [[ -n "$ENROLL_TOKEN" ]]; then
  echo ">> Writing enrollment token to ${CONFIG_DIR}/enroll.env"
  printf 'LOOM_ENROLL_TOKEN=%s\n' "$ENROLL_TOKEN" > "${CONFIG_DIR}/enroll.env"
  chmod 600 "${CONFIG_DIR}/enroll.env"
  [[ "$OS" == "Linux" ]] && chown root:loom-client "${CONFIG_DIR}/enroll.env"
fi

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
ReadWritePaths=/var/lib/loom-client

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
