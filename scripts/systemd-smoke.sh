#!/usr/bin/env bash
#
# Manual systemd smoke test.
#
# Deliberately not in CI: the things that actually break -- mount-namespace
# visibility to your file manager, PAM keyring unlock, TPM access from a
# lingering user manager, fusermount3's setuid behaviour under the host's LSM
# policy -- are precisely what a privileged container fakes. A real desktop
# session is a strictly better test environment.
#
# Usage: scripts/systemd-smoke.sh <mount-name>

set -euo pipefail

NAME="${1:-}"
if [[ -z "$NAME" ]]; then
    echo "usage: $0 <mount-name>" >&2
    exit 64
fi

UNIT="davfs-sync@${NAME}.service"
step() { printf '\n=== %s ===\n' "$1"; }
ok()   { printf '  ok: %s\n' "$1"; }
bad()  { printf '  FAIL: %s\n' "$1" >&2; FAILED=1; }
FAILED=0

step "unit files"
if systemctl --user cat "$UNIT" >/dev/null 2>&1; then
    ok "$UNIT is known to systemd"
else
    bad "$UNIT not found; install systemd/*.service and *.target into ~/.config/systemd/user/"
    exit 69
fi

step "drop-in"
DROPIN="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/${UNIT}.d/50-davfs-sync.conf"
if [[ -f "$DROPIN" ]]; then
    ok "drop-in present at $DROPIN"
    MOUNT_POINT="$(sed -n 's/^Environment="\?DAVFS_MOUNT_POINT=\([^"]*\)"\?$/\1/p' "$DROPIN" | tail -1)"
    MOUNT_POINT="${MOUNT_POINT//%%/%}"
    ok "mount point: ${MOUNT_POINT:-<unset>}"
else
    bad "no drop-in; run: davfs-sync setup $NAME ..."
    exit 69
fi

step "start"
systemctl --user start "$UNIT"
sleep 2
STATE="$(systemctl --user is-active "$UNIT" || true)"
if [[ "$STATE" == "active" ]]; then
    ok "unit is active (not merely activating: Type=notify readiness works)"
else
    bad "unit is $STATE"
    systemctl --user status "$UNIT" --no-pager | tail -20
    exit 69
fi

# The single most important check. If a sandboxing directive ever creeps into
# the unit, the mount succeeds inside a private namespace and is invisible here.
step "the mount is visible OUTSIDE the service"
if [[ -n "$MOUNT_POINT" ]] && mountpoint -q "$MOUNT_POINT"; then
    ok "$MOUNT_POINT is a mount point in this shell"
    if ls "$MOUNT_POINT" >/dev/null 2>&1; then
        ok "listing works: $(find "$MOUNT_POINT" -mindepth 1 -maxdepth 1 | wc -l) entries"
    else
        bad "cannot list $MOUNT_POINT"
    fi
else
    bad "$MOUNT_POINT is NOT visible here -- a Protect*/PrivateMounts directive has probably crept into the unit"
fi

step "reading a file"
FIRST_FILE="$(find "$MOUNT_POINT" -maxdepth 2 -type f 2>/dev/null | head -1 || true)"
if [[ -n "$FIRST_FILE" ]]; then
    if head -c 64 "$FIRST_FILE" >/dev/null 2>&1; then
        ok "read $(basename "$FIRST_FILE")"
    else
        bad "could not read $FIRST_FILE"
    fi
else
    printf '  skip: no regular file found to read\n'
fi

step "extended attributes"
if [[ -n "$FIRST_FILE" ]] && command -v getfattr >/dev/null; then
    getfattr -d -m 'user\.davfs\.' "$FIRST_FILE" 2>/dev/null | sed 's/^/  /' || bad "getfattr failed"
else
    printf '  skip: getfattr not installed\n'
fi

step "readiness ordering"
systemd-analyze --user blame 2>/dev/null | grep -F "$UNIT" | sed 's/^/  /' || true
journalctl --user -u "$UNIT" --no-pager -n 5 | sed 's/^/  /'

step "stop"
systemctl --user stop "$UNIT"
sleep 1
if [[ -n "$MOUNT_POINT" ]] && mountpoint -q "$MOUNT_POINT"; then
    bad "$MOUNT_POINT is still mounted after stop"
else
    ok "unmounted cleanly"
fi

printf '\n'
if [[ "$FAILED" -eq 0 ]]; then
    echo "all checks passed"
else
    echo "some checks FAILED" >&2
    exit 1
fi
