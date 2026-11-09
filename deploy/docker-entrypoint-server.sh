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

exec runuser -u loom -- /usr/local/bin/loom-server "$@"
