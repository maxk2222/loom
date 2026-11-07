#!/usr/bin/env bash
# LOOM client installer — installs loom-client + loom CLI, config and systemd unit.
# Usage:
#   curl -fsSL https://<server>/install-client.sh | sudo bash -s -- --server https://loom.example.com:9443 --token enroll_...
set -euo pipefail

SERVER_URL=""
ENROLL_TOKEN=""
CLIENT_NAME="$(hostname -s 2>/dev/null || hostname)"
PREFIX="/usr/local"
CONFIG_DIR="/etc/loom-client"
DATA_DIR="/var/lib/loom-client"
OS="$(uname -s)"

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
[[ -n "$SERVER_URL" ]] || usage

if [[ "$EUID" -ne 0 ]] && [[ "$OS" == "Linux" ]]; then
  echo "Please run as root (systemd + ${CONFIG_DIR})." >&2
  exit 1
fi

BIN_DIR="${PREFIX}/bin"
echo ">> Installing binaries to ${BIN_DIR}"
# Binaries are expected alongside this script (release bundle) or on PATH.
for bin in loom-client loom; do
  if command -v "$bin" >/dev/null 2>&1; then
    src="$(command -v "$bin")"
  elif [[ -x "./bin/$bin" ]]; then
    src="./bin/$bin"
  else
    echo "!! $bin not found — place release binaries under ./bin/ or PATH" >&2
    exit 1
  fi
  install -m 0755 "$src" "${BIN_DIR}/${bin}"
done

if [[ "$OS" == "Linux" ]]; then
  echo ">> Creating ${CONFIG_DIR}, ${DATA_DIR}"
  mkdir -p "$CONFIG_DIR" "$DATA_DIR"
  chmod 750 "$CONFIG_DIR"
  id -u loom-client &>/dev/null || useradd -r -d "$DATA_DIR" -s /usr/sbin/nologin loom-client
  chown -R loom-client:loom-client "$DATA_DIR"
else
  CONFIG_DIR="${HOME}/Library/Application Support/loom-client"
  mkdir -p "$CONFIG_DIR"
fi

CFG="${CONFIG_DIR}/config.toml"
if [[ ! -f "$CFG" ]]; then
  echo ">> Writing $CFG"
  cat > "$CFG" <<EOF
name = "${CLIENT_NAME}"
server_url = "${SERVER_URL}"
local_api_bind = "127.0.0.1:8391"
admin_bind = "127.0.0.1:8391"

[sharing]
enabled = true
default_mode = "auto"

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
ExecStart=/usr/local/bin/loom-client
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
