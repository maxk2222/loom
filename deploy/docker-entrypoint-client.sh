#!/bin/sh
# LOOM client container entrypoint.
#
# On first start (no config.toml on the volume) a config is rendered from
# environment variables. Once a config exists it is left untouched — edit it
# directly or manage the client through its dashboard/local API.
#
#   LOOM_SERVER_URL           gRPC endpoint of the LOOM server (required)
#   LOOM_ENROLL_TOKEN         enrolment token for the first connect
#   LOOM_CLIENT_NAME          display name (default: container hostname)
#   LOOM_LOCAL_API_BIND       local API bind (default 0.0.0.0:8391 so the
#                           dashboard is reachable through the port mapping —
#                           set LOOM_LOCAL_API_KEY or keep the port on loopback)
#   LOOM_LOCAL_API_KEY        optional bearer token for the local API
#   LOOM_SHARING              share capacity with the pool (default true)
#   LOOM_PRESSURE_CPU / LOOM_PRESSURE_RAM   gate tier-2 above these host % (0=off)
#   LOOM_UNLOAD_MODELS        also unload idle models while gated (default false)
#   LOOM_PRESSURE_COOLDOWN    seconds below limits before reopening (default 30)
#
# One-backend quick bootstrap (more backends: mount your own config.toml):
#   LOOM_BACKEND_TYPE   ollama | llamacpp | generic_openai | vllm | azure_openai | anthropic
#   LOOM_BACKEND_URL    e.g. http://host.docker.internal:11434
#   LOOM_MODELS         comma-separated physical model names
#   LOOM_BACKEND_KEY    optional API key for the backend itself
set -eu

CFG="${LOOM_CONFIG:-/etc/loom-client/config.toml}"

# Strip characters that would break the TOML string literals below.
esc() { printf '%s' "$1" | tr -d '"\\'; }
# Numeric env → digits only (invalid input falls back to the default).
num() { case "$1" in ''|*[!0-9]*) echo "$2" ;; *) echo "$1" ;; esac; }
# Boolean env → strict true/false (anything else falls back to the default).
bool() { case "$1" in true|false) echo "$1" ;; *) echo "$2" ;; esac; }

if [ ! -f "$CFG" ]; then
    if [ -z "${LOOM_SERVER_URL:-}" ]; then
        echo "loom-client: LOOM_SERVER_URL is required on first start" >&2
        echo "             (or mount an existing config.toml to $CFG)" >&2
        exit 1
    fi

    NAME="$(esc "${LOOM_CLIENT_NAME:-$(hostname)}")"
    BIND="$(esc "${LOOM_LOCAL_API_BIND:-0.0.0.0:8391}")"
    SHARING="$(bool "${LOOM_SHARING:-}" true)"
    CPU="$(num "${LOOM_PRESSURE_CPU:-}" 0)"; [ "$CPU" -le 100 ] || CPU=100
    RAM="$(num "${LOOM_PRESSURE_RAM:-}" 0)"; [ "$RAM" -le 100 ] || RAM=100
    UNLOAD="$(bool "${LOOM_UNLOAD_MODELS:-}" false)"
    COOLDOWN="$(num "${LOOM_PRESSURE_COOLDOWN:-}" 30)"

    mkdir -p "$(dirname "$CFG")" 2>/dev/null || {
        echo "loom-client: cannot create $(dirname "$CFG") — check volume permissions" >&2
        exit 1
    }
    {
        echo "name = \"$NAME\""
        echo "server_url = \"$(esc "$LOOM_SERVER_URL")\""
        echo "connect_enabled = true"
        echo "sharing_enabled = $SHARING"
        echo ""
        echo "[local_api]"
        echo "bind = \"$BIND\""
        echo "api_key = \"$(esc "${LOOM_LOCAL_API_KEY:-}")\""
        echo "serve_ui = true"
        echo ""
        echo "[pressure]"
        echo "enabled = true"
        echo "cpu_pct = $CPU"
        echo "ram_pct = $RAM"
        echo "unload_models = $UNLOAD"
        echo "cooldown_secs = $COOLDOWN"
        echo ""
        # Self-update makes no sense inside a container — pull a newer image.
        echo "[updater]"
        echo "enabled = false"
        echo ""
        echo "[tray]"
        echo "enabled = false"

        if [ -n "${LOOM_BACKEND_URL:-}" ]; then
            BTYPE="${LOOM_BACKEND_TYPE:-ollama}"
            BID="$(cat /proc/sys/kernel/random/uuid)"
            echo ""
            echo "[[backends]]"
            echo "id = \"$BID\""
            echo "name = \"$(esc "${LOOM_BACKEND_NAME:-docker-backend}")\""
            echo "backend_type = \"$(esc "$BTYPE")\""
            echo "endpoint = \"$(esc "$LOOM_BACKEND_URL")\""
            echo "api_key = \"$(esc "${LOOM_BACKEND_KEY:-}")\""
            echo "enabled = true"
            OLDIFS="$IFS"; IFS=','
            for MODEL in ${LOOM_MODELS:-}; do
                MODEL="$(echo "$MODEL" | tr -d '[:space:]' | tr -d '"\\')"
                [ -n "$MODEL" ] || continue
                DID="$(cat /proc/sys/kernel/random/uuid)"
                echo ""
                echo "[[backends.deployments]]"
                echo "id = \"$DID\""
                echo "name = \"$MODEL\""
                echo "model_name = \"$MODEL\""
                echo "max_concurrent = 4"
                echo "local_reserve = 1"
                echo "tier2_max = 2"
                echo "sharing_mode = \"shared\""
                echo "auto_load = true"
                echo "enabled = true"
            done
            IFS="$OLDIFS"
        fi
    } > "$CFG"
    chmod 600 "$CFG"
    echo "loom-client: wrote initial config to $CFG" >&2
fi

exec /usr/local/bin/loom-client "$@"
