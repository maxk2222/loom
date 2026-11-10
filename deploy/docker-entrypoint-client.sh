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

# The container starts as root so state/bind-mounted dirs can be
# ownership-healed regardless of the host-side uid, then drops to `loom`.
if [ "$(id -u)" = "0" ]; then
    mkdir -p "$(dirname "$CFG")"
    # Seed files (config.toml, ca.crt, …) from a read-only host mount into the
    # state dir. Copies on first boot and whenever the seed file is newer —
    # a re-run of setup-client.sh refreshes config.toml without wiping state.
    SEED="${LOOM_SEED_DIR:-/etc/loom-seed}"
    if [ -d "$SEED" ]; then
        for f in "$SEED"/*; do
            [ -f "$f" ] || continue
            dest="$(dirname "$CFG")/$(basename "$f")"
            if [ ! -f "$dest" ] || [ "$f" -nt "$dest" ]; then
                cp "$f" "$dest"
                echo "loom-client: seeded $(basename "$f")" >&2
            fi
        done
    fi
    chown -R loom:loom "$(dirname "$CFG")" 2>/dev/null || true
    chmod 700 "$(dirname "$CFG")" 2>/dev/null || true
    [ -f "$CFG" ] && chmod 600 "$CFG" 2>/dev/null || true
fi

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
    case "$BIND" in
        127.0.0.1:*|localhost:*|\[::1\]:*) ;;
        *) [ -n "${LOOM_LOCAL_API_KEY:-}" ] || \
            echo "loom-client: WARNING — local API binds $BIND with no LOOM_LOCAL_API_KEY set; unauthenticated from the network" >&2 ;;
    esac
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
        # The binary runs from the state volume (see below), so self-update
        # works inside the container: swap → exit → restart policy.
        echo "[updater]"
        echo "enabled = true"
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
    [ "$(id -u)" = "0" ] && chown loom:loom "$CFG" 2>/dev/null || true
    echo "loom-client: wrote initial config to $CFG" >&2
fi

# Self-update support: run the binary from the persistent state dir so the
# updater's atomic swap survives `docker compose up` recreates and the
# restart policy acts as the supervisor. /usr/local/bin/loom-client stays
# the immutable image seed. Re-seed when the image is strictly newer
# (self-updated volume binaries are newer than the image and are kept).
STATE_DIR="$(dirname "$CFG")"
BIN="$STATE_DIR/bin/loom-client"
SEED_BIN=/usr/local/bin/loom-client
# Version decides first — a self-updated volume binary has a HIGHER version
# than the image and must be kept. On equal versions the newer build stamp
# wins, so same-version image rebuilds still propagate. A missing volume
# stamp predates build stamping and counts as older.
ver() { "$1" --version 2>/dev/null | awk '{print $NF}'; }
IMG_ID="$(cat /etc/loom-build-id 2>/dev/null || echo 0)"
# Swap via rename, not in-place cp: overwriting the file a still-running
# process executes fails with ETXTBSY and would silently keep the stale bin.
seed() { cp "$SEED_BIN" "$BIN.new" && mv -f "$BIN.new" "$BIN" && echo "$IMG_ID" > "$BIN.build-id"; }
if [ -x "$BIN" ]; then
    IMG_V="$(ver "$SEED_BIN")"; VOL_V="$(ver "$BIN")"
    NEWER="$(printf '%s\n%s\n' "$IMG_V" "$VOL_V" | sort -V | tail -1)"
    VOL_ID="$(cat "$BIN.build-id" 2>/dev/null || echo 0)"
    case "$VOL_ID" in ''|*[!0-9]*) VOL_ID=0 ;; esac
    if [ -n "$IMG_V" ] && [ "$IMG_V" != "$VOL_V" ] && [ "$NEWER" = "$IMG_V" ]; then
        seed && echo "loom-client: re-seeded binary v$IMG_V (volume had v$VOL_V)" >&2 \
            || echo "loom-client: re-seed FAILED — running volume binary" >&2
    elif [ "$IMG_V" = "$VOL_V" ] && [ "$IMG_ID" -gt "$VOL_ID" ]; then
        seed && echo "loom-client: re-seeded same-version rebuild $IMG_ID (was $VOL_ID)" >&2 \
            || echo "loom-client: re-seed FAILED — running volume binary" >&2
    fi
else
    mkdir -p "$STATE_DIR/bin"
    seed
fi
[ "$(id -u)" = "0" ] && chown -R loom:loom "$STATE_DIR/bin" 2>/dev/null || true

if [ "$(id -u)" = "0" ]; then
    exec runuser -u loom -- "$BIN" "$@"
else
    exec "$BIN" "$@"
fi
