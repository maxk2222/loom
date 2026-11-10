#!/bin/sh
# LOOM server container entrypoint.
#
# Starts as root so state/bind-mounted directories can be ownership-healed
# regardless of the host-side uid, then drops to the unprivileged `loom`
# user (uid/gid 10001). Bind mounts keep their host permissions otherwise —
# a classic fresh-install failure mode.
set -eu

LOOM_UID=10001
LOOM_GID=10001

for d in /data /var/lib/loom; do
    if [ -d "$d" ]; then
        chown -R "$LOOM_UID:$LOOM_GID" "$d" 2>/dev/null || true
    else
        mkdir -p "$d" && chown "$LOOM_UID:$LOOM_GID" "$d" 2>/dev/null || true
    fi
done

# Self-update support: run the binary from the persistent /data volume so the
# updater's atomic swap survives container recreates and the restart policy
# acts as the supervisor. /usr/local/bin/loom-server is the immutable image
# seed — re-seed only when the image is strictly newer.
BIN=/data/bin/loom-server
SEED_BIN=/usr/local/bin/loom-server
ver() { "$1" --version 2>/dev/null | awk '{print $NF}'; }
if [ -x "$BIN" ]; then
    IMG_V="$(ver "$SEED_BIN")"; VOL_V="$(ver "$BIN")"
    NEWER="$(printf '%s\n%s\n' "$IMG_V" "$VOL_V" | sort -V | tail -1)"
    if [ -n "$IMG_V" ] && [ "$IMG_V" != "$VOL_V" ] && [ "$NEWER" = "$IMG_V" ]; then
        cp "$SEED_BIN" "$BIN" && echo "loom-server: re-seeded binary v$IMG_V (volume had v$VOL_V)" >&2
    fi
else
    mkdir -p /data/bin
    cp "$SEED_BIN" "$BIN"
fi
chown -R "$LOOM_UID:$LOOM_GID" /data/bin 2>/dev/null || true

exec runuser -u loom -- "$BIN" "$@"
