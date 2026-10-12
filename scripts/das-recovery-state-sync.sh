#!/bin/bash
# das-recovery-state-sync.sh — on a recovery drive's own OS, at boot: the
# newest received copy of btrdasd's state (index, records, history) reflinked
# into /var/lib/das-backup, and the host's config.toml, from the newest
# received root snapshot, derived into this OS's /etc/das-backup/config.toml
# (btrdasd config derive-recovery). Same btrfs: the copy is instant and costs
# no space. Then, only when the state is older than the data (a night the
# state pass failed), an incremental index catch-up in the background.
# Version: 1.0.0
# Date: 2026-10-12
#
# Never runs on the host: ConditionPathExists=/etc/das-backup/recovery-drive
# in the unit, and the marker is written only by provisioning. A failure
# leaves whatever /var/lib/das-backup held — never an empty database — and
# exits 1 with the reason in the journal and in $STATUS_FILE.
set -euo pipefail

ROOT="${DAS_STATE_SYNC_ROOT:-}" # test seam: a fake filesystem tree
STATE_DIR="${DAS_STATE_SYNC_STATE_DIR:-$ROOT/var/lib/das-backup}"
CONFIG_OUT="${DAS_STATE_SYNC_CONFIG_OUT:-$ROOT/etc/das-backup/config.toml}"
MARKER="${DAS_STATE_SYNC_MARKER:-$ROOT/etc/das-backup/recovery-drive}"
RUN_DIR="${DAS_STATE_SYNC_RUN_DIR:-$ROOT/run/das-recovery-state}"
TOP="${DAS_STATE_SYNC_TOP:-$RUN_DIR/top}" # the booted fs mounted at subvolid=5
BTRDASD="${DAS_STATE_SYNC_BTRDASD:-/usr/bin/btrdasd}"
STATUS_FILE="$RUN_DIR/status"

say() { printf '%s\n' "$*"; }
status() {
    mkdir -p "$RUN_DIR"
    printf '%s\n' "$*" >"$STATUS_FILE"
}
# die <reason>: the reason goes to stderr (journal) and the status file.
die() {
    say "das-recovery-state-sync: $*" >&2
    status "failed: $*"
    exit 1
}
cleanup_new() { rm -rf "$STATE_DIR.new" "$CONFIG_OUT.new"; }

# The marker gate writes nothing at all, not even a status file: on the host
# this must be a pure refusal.
if [[ ! -e "$MARKER" ]]; then
    say "das-recovery-state-sync: not a recovery drive ($MARKER absent): nothing done" >&2
    exit 1
fi

uuid=$(findmnt -no UUID "${ROOT:-/}") || die "cannot read the booted filesystem's UUID"
[[ -n "$uuid" ]] || die "the booted filesystem has no UUID"
src=$(findmnt -no SOURCE "${ROOT:-/}") || die "cannot read the booted filesystem's source device"
src=${src%%\[*}
mkdir -p "$TOP"
if [[ -z "$ROOT" ]]; then
    mount -o subvolid=5,ro "$src" "$TOP" || die "cannot mount the booted filesystem's top level read-only"
    trap 'umount "$TOP" 2>/dev/null || :' EXIT
fi

# The drive's own received directory for the nvme volume: the top-level
# directory named like the target's subdir.
dir="$TOP/nvme"
[[ -d "$dir" ]] || die "no received 'nvme' directory on this filesystem: not a target that receives the host's root"

# newest <prefix>: the lexically greatest snapshot directory (stamps sort).
newest() {
    local c=("$dir/$1".*)
    [[ -e "${c[0]}" ]] || return 0
    printf '%s\n' "${c[@]}" | sort | tail -n1
}
state_snap=$(newest das-state)
root_snap=$(newest root-)

[[ -n "$root_snap" ]] || die "no received root- snapshot: the host's config.toml cannot be derived"
[[ -r "$root_snap/etc/das-backup/config.toml" ]] || die "the newest root snapshot ${root_snap##*/} has no etc/das-backup/config.toml"
if [[ -z "$state_snap" ]]; then
    # Not die(): the status word for this case is exactly "no state snapshot".
    say "das-recovery-state-sync: no received das-state snapshot yet (the next backup run sends one): state left as it was" >&2
    status "no state snapshot"
    exit 1
fi

# Config first: derive-recovery refuses before anything is replaced when the
# UUID is unknown or ambiguous.
mkdir -p "$(dirname "$CONFIG_OUT")"
rm -f "$CONFIG_OUT.new"
if ! "$BTRDASD" config derive-recovery --from "$root_snap/etc/das-backup/config.toml" \
    --booted-uuid "$uuid" --out "$CONFIG_OUT.new"; then
    cleanup_new
    die "derive-recovery refused (see above): state left as it was"
fi

# State: reflink (or plain copy) into a sibling, then swap — the old copy is
# never half-replaced.
rm -rf "$STATE_DIR.new"
mkdir -p "$STATE_DIR.new"
if ! cp -a --reflink=auto "$state_snap/." "$STATE_DIR.new/"; then
    cleanup_new
    die "copying ${state_snap##*/} failed: state left as it was"
fi
if [[ -d "$STATE_DIR" ]]; then
    rm -rf "$STATE_DIR.old"
    mv "$STATE_DIR" "$STATE_DIR.old"
fi
mv "$STATE_DIR.new" "$STATE_DIR"
rm -rf "$STATE_DIR.old"
mv "$CONFIG_OUT.new" "$CONFIG_OUT"

stamp=${state_snap##*.}
data_stamp=${root_snap##*.}
if [[ "$stamp" < "$data_stamp" ]]; then
    # The state is older than the data: catch up in the background.
    status "synced $stamp; catch-up index running (data $data_stamp)"
    say "state $stamp is older than the newest data $data_stamp: starting an incremental index catch-up"
    systemd-run --quiet --collect --unit=das-recovery-state-catchup --property=Type=oneshot \
        "$BTRDASD" walk "$TOP/nvme" --db "$STATE_DIR/backup-index.db" --config "$CONFIG_OUT" ||
        say "could not start the catch-up (systemd-run failed): search is current to $stamp only" >&2
else
    status "synced $stamp"
fi
say "state synced from ${state_snap##*/}; configuration derived from ${root_snap##*/}"
