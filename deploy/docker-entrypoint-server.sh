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
# seed. Version decides first — a self-updated volume binary has a HIGHER
# version than the image and must be kept. On equal versions the newer build
# stamp wins, so same-version image rebuilds still propagate. A missing
# volume stamp predates build stamping and counts as older.
BIN=/data/bin/loom-server
SEED_BIN=/usr/local/bin/loom-server
OKF="$BIN.boot-ok"
ATF="$BIN.boot-attempt"
ver() { "$1" --version 2>/dev/null | awk '{print $NF}'; }
IMG_ID="$(cat /etc/loom-build-id 2>/dev/null || echo 0)"
# Swap via rename, not in-place cp: overwriting the file a still-running
# process executes fails with ETXTBSY and would silently keep the stale bin.
# A re-seed resets provenance: the image is the new baseline, and stale
# .prev/.boot-ok/.boot-attempt from self-update days must never outrank it.
seed() { cp "$SEED_BIN" "$BIN.new" && mv -f "$BIN.new" "$BIN" && echo "$IMG_ID" > "$BIN.build-id" \
    && rm -f "$BIN.prev" "$OKF" "$ATF"; }
# Intentional downgrades: the version guard below protects self-updated
# binaries from older images — LOOM_FORCE_RESEED=1 is the documented escape
# hatch to pin the image's version back onto the volume.
if [ "${LOOM_FORCE_RESEED:-}" = "1" ]; then
    mkdir -p /data/bin
    seed && echo "loom-server: forced re-seed to v$(ver "$SEED_BIN") (LOOM_FORCE_RESEED=1)" >&2 \
        || echo "loom-server: forced re-seed FAILED — running volume binary" >&2
elif [ -x "$BIN" ]; then
    IMG_V="$(ver "$SEED_BIN")"; VOL_V="$(ver "$BIN")"
    # Only dotted-digit strings count as versions: garbage output sorts
    # ABOVE real versions under sort -V and would silently keep a broken
    # volume binary. Unparseable volume versions count as 0 → re-seeded.
    case "$IMG_V" in *[!0-9.]*) IMG_V="" ;; esac
    case "$VOL_V" in *[!0-9.]*) VOL_V=0 ;; esac
    NEWER="$(printf '%s\n%s\n' "$IMG_V" "$VOL_V" | sort -V | tail -1)"
    VOL_ID="$(cat "$BIN.build-id" 2>/dev/null || echo 0)"
    case "$VOL_ID" in ''|*[!0-9]*) VOL_ID=0 ;; esac
    if [ -n "$IMG_V" ] && [ "$IMG_V" != "$VOL_V" ] && [ "$NEWER" = "$IMG_V" ]; then
        seed && echo "loom-server: re-seeded binary v$IMG_V (volume had v$VOL_V)" >&2 \
            || echo "loom-server: re-seed FAILED — running volume binary" >&2
    elif [ "$IMG_V" = "$VOL_V" ] && [ "$IMG_ID" -gt "$VOL_ID" ]; then
        seed && echo "loom-server: re-seeded same-version rebuild $IMG_ID (was $VOL_ID)" >&2 \
            || echo "loom-server: re-seed FAILED — running volume binary" >&2
    fi
else
    mkdir -p /data/bin
    # Crash between the self-update's two renames (exe→prev, new→exe) leaves
    # a staged .new with no BIN — completing the swap is strictly better than
    # falling back to the image: the staged binary was already sig-verified.
    if [ -x "$BIN.new" ]; then
        mv -f "$BIN.new" "$BIN" \
            && echo "loom-server: completed interrupted self-update swap to v$(ver "$BIN")" >&2
    else
        seed
    fi
fi
chown -R "$LOOM_UID:$LOOM_GID" /data/bin 2>/dev/null || true

# Auto-rollback: .boot-ok names the version that last reached a healthy
# startup (the running server writes it after all listeners bind);
# .boot-attempt tracks "<version>:<unproven boots>" — incremented only
# while the marker disagrees, so a binary that proved itself once keeps
# normal restart retries forever. Roll back when the current binary failed
# to prove itself twice AND .prev IS the proven one — a crashing
# self-update self-heals instead of crash-looping, while a transient boot
# failure still gets a retry. Two unproven binaries never ping-pong:
# rollback requires prev == marker.
CUR_V="$(ver "$BIN")"; PROVEN="$(cat "$OKF" 2>/dev/null || true)"
ATT="$(cat "$ATF" 2>/dev/null || true)"; ATT_V="${ATT%%:*}"; ATT_N="${ATT##*:}"
case "$ATT_N" in ''|*[!0-9]*) ATT_N=0 ;; esac
if [ "$ATT_V" = "$CUR_V" ]; then ATT_N=$((ATT_N + 1)); else ATT_N=1; fi
if [ -x "$BIN.prev" ] && [ -n "$PROVEN" ] && [ "$CUR_V" != "$PROVEN" ] \
    && [ "$(ver "$BIN.prev")" = "$PROVEN" ] && [ "$ATT_N" -ge 3 ]; then
    echo "loom-server: rolled back unproven v$CUR_V -> proven v$PROVEN" >&2
    mv -f "$BIN.prev" "$BIN"; CUR_V="$PROVEN"; ATT_N=1
fi
echo "$CUR_V:$ATT_N" > "$ATF" 2>/dev/null || true
# A lingering .new while BIN is intact is leftover staging from an aborted
# update — never adoptable.
rm -f "$BIN.new"

exec runuser -u loom -- "$BIN" "$@"
