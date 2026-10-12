#!/bin/bash
# das-recovery-state-sync.sh — on a recovery drive's own OS, at boot: the
# newest received copy of btrdasd's state (index, records, history) reflinked
# into /var/lib/das-backup, and the host's config.toml, from the newest
# received root snapshot, derived into this OS's /etc/das-backup/config.toml
# (btrdasd config derive-recovery). Same btrfs: the copy is instant and costs
# no space. Then, only when the state is older than the data (a night the
# state pass failed), an incremental index catch-up in the background.
# Version: 1.0.2
# Date: 2026-10-12
#
# Never runs on the host: ConditionPathExists=/etc/das-backup/recovery-drive
# in the unit, and the marker is written only by provisioning. A failure
# leaves whatever /var/lib/das-backup held — never an empty database — and
# exits 1 with the reason in the journal and in $STATUS_FILE. Exit 2 (the unit
# lists it in SuccessExitStatus) is "no state snapshot yet": a normal first
# boot before any backup, not a failure.
#
# The catch-up walks the drive's OWN target, so that target is mounted here at
# subvolid=5 (its top level, by the booted UUID) before walk starts: walk then
# finds it mounted and never binds the running `/` (subvolume @) at its path.
# It is left mounted for the rest of the recovery session (the GUI restore
# reads it); it is deliberately not in the EXIT trap.
set -euo pipefail

ROOT="${DAS_STATE_SYNC_ROOT:-}" # test seam: a fake filesystem tree
STATE_DIR="${DAS_STATE_SYNC_STATE_DIR:-$ROOT/var/lib/das-backup}"
CONFIG_OUT="${DAS_STATE_SYNC_CONFIG_OUT:-$ROOT/etc/das-backup/config.toml}"
MARKER="${DAS_STATE_SYNC_MARKER:-$ROOT/etc/das-backup/recovery-drive}"
RUN_DIR="${DAS_STATE_SYNC_RUN_DIR:-$ROOT/run/das-recovery-state}"
TOP="${DAS_STATE_SYNC_TOP:-$RUN_DIR/top}" # the booted fs mounted at subvolid=5
BTRDASD="${DAS_STATE_SYNC_BTRDASD:-/usr/bin/btrdasd}"
# Every external tool is a named seam (default: the bare name, found on PATH).
FINDMNT="${DAS_STATE_SYNC_FINDMNT:-findmnt}"
SYSTEMD_RUN="${DAS_STATE_SYNC_SYSTEMD_RUN:-systemd-run}"
MOUNT="${DAS_STATE_SYNC_MOUNT:-mount}"
UMOUNT="${DAS_STATE_SYNC_UMOUNT:-umount}"
BTRFS="${DAS_STATE_SYNC_BTRFS:-btrfs}"
MV="${DAS_STATE_SYNC_MV:-mv}"
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

uuid=$("$FINDMNT" -no UUID "${ROOT:-/}") || die "cannot read the booted filesystem's UUID"
[[ -n "$uuid" ]] || die "the booted filesystem has no UUID"
src=$("$FINDMNT" -no SOURCE "${ROOT:-/}") || die "cannot read the booted filesystem's source device"
src=${src%%\[*}
mkdir -p "$TOP"
if [[ -z "$ROOT" ]]; then
    "$MOUNT" -o subvolid=5,ro "$src" "$TOP" || die "cannot mount the booted filesystem's top level read-only"
    trap '"$UMOUNT" "$TOP" 2>/dev/null || :' EXIT
fi

# The drive's own received directory for the nvme volume: the top-level
# directory named like the target's subdir.
dir="$TOP/nvme"
[[ -d "$dir" ]] || die "no received 'nvme' directory on this filesystem: not a target that receives the host's root"

# complete <path>: a finished receive is a read-only subvolume with a
# Received UUID. A partial receive (interrupted btrfs receive) is writable and
# has no Received UUID.
complete() {
    local info
    info=$("$BTRFS" subvolume show "$1" 2>/dev/null) || return 1
    grep -Eq '^[[:space:]]*Flags:.*readonly' <<<"$info" || return 1
    awk '$1 == "Received" && $2 == "UUID:" { found = ($3 != "-" && $3 != "") } END { exit !found }' <<<"$info"
}

# newest <prefix>: the lexically greatest COMPLETE snapshot directory (stamps
# sort); incomplete candidates are skipped with a logged line.
newest() {
    local c=("$dir/$1".*) i
    [[ -e "${c[0]}" ]] || return 0
    mapfile -t c < <(printf '%s\n' "${c[@]}" | sort -r)
    for i in "${c[@]}"; do
        if complete "$i"; then
            printf '%s\n' "$i"
            return 0
        fi
        say "das-recovery-state-sync: skipping ${i##*/}: not a completed receive (not read-only or no Received UUID)" >&2
    done
}

state_snap=$(newest das-state)
root_snap=$(newest root-)

[[ -n "$root_snap" ]] || die "no received root- snapshot: the host's config.toml cannot be derived"
[[ -r "$root_snap/etc/das-backup/config.toml" ]] || die "the newest root snapshot ${root_snap##*/} has no etc/das-backup/config.toml"
if [[ -z "$state_snap" ]]; then
    # Not die(): the status word for this case is exactly "no state snapshot".
    say "das-recovery-state-sync: no received das-state snapshot yet (the next backup run sends one): state left as it was" >&2
    status "no state snapshot"
    exit 2
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
# never half-replaced. Every step has its own failure branch: the previous
# state stays in place, *.new is removed, the status says why.
rm -rf "$STATE_DIR.new"
mkdir -p "$STATE_DIR.new" || die "cannot create $STATE_DIR.new: state left as it was"
if ! cp -a --reflink=auto "$state_snap/." "$STATE_DIR.new/"; then
    cleanup_new
    die "copying ${state_snap##*/} failed: state left as it was"
fi
if [[ ! -d "$STATE_DIR" ]]; then
    "$MV" "$STATE_DIR.new" "$STATE_DIR" || {
        cleanup_new
        die "installing the new state failed: state left as it was"
    }
elif mv_help=$("$MV" --help 2>&1) && grep -q -- --exchange <<<"$mv_help"; then
    # One atomic step: $STATE_DIR is never absent.
    "$MV" -T --exchange "$STATE_DIR.new" "$STATE_DIR" || {
        cleanup_new
        die "swapping the new state in failed: state left as it was"
    }
    rm -rf "$STATE_DIR.new" || say "das-recovery-state-sync: could not remove the replaced state ($STATE_DIR.new)" >&2
else
    # No mv --exchange (coreutils < 9.5): two steps, undone if the second fails.
    rm -rf "$STATE_DIR.old"
    "$MV" "$STATE_DIR" "$STATE_DIR.old" || {
        cleanup_new
        die "setting the old state aside failed: state left as it was"
    }
    "$MV" "$STATE_DIR.new" "$STATE_DIR" || {
        cleanup_new
        "$MV" "$STATE_DIR.old" "$STATE_DIR" ||
            die "swap failed and the previous state could not be restored: it is in $STATE_DIR.old"
        die "installing the new state failed: previous state restored"
    }
    rm -rf "$STATE_DIR.old" || say "das-recovery-state-sync: could not remove $STATE_DIR.old" >&2
fi
"$MV" "$CONFIG_OUT.new" "$CONFIG_OUT" || {
    cleanup_new
    die "installing the derived configuration failed: state is new, configuration is the previous one"
}

# top_level_of <mountpoint>: "UUID FSROOT" of what is mounted there; empty and
# nonzero when nothing is.
top_level_of() { "$FINDMNT" -no UUID,FSROOT -M "$1" 2>/dev/null; }

# ensure_own_target <mountpoint>: this drive's own filesystem mounted there at
# its top level, read-only. Already a mountpoint: its UUID must be this
# drive's and its FSROOT "/", else die (a bind of the running `/` would make
# every snapshot path under it absent). Returns 1 if the mount itself fails;
# the result is read back before it is believed.
ensure_own_target() {
    local mp=$1 seen u r
    mkdir -p "$mp" || return 1
    if seen=$(top_level_of "$mp") && [[ -n "$seen" ]]; then
        read -r u r <<<"$seen"
        [[ "${u,,}" == "${uuid,,}" && "$r" == "/" ]] ||
            die "$walk_mount is mounted but not this drive's top level (uuid ${u:-unknown}, fsroot ${r:-unknown})"
        return 0
    fi
    "$MOUNT" -o subvolid=5,ro "UUID=$uuid" "$mp" || return 1
    seen=$(top_level_of "$mp") || return 1
    read -r u r <<<"$seen"
    [[ "${u,,}" == "${uuid,,}" && "$r" == "/" ]] ||
        die "$walk_mount is mounted but not this drive's top level (uuid ${u:-unknown}, fsroot ${r:-unknown})"
}

stamp=${state_snap##*.}
data_stamp=${root_snap##*.}
if [[ "$stamp" < "$data_stamp" ]]; then
    # The state is older than the data: catch up in the background.
    say "state $stamp is older than the newest data $data_stamp: starting an incremental index catch-up"
    # walk wants a target ROOT (it holds source dirs such as nvme/). The own
    # target was mounted above at subvolid=5 (read-only, by UUID) and verified,
    # so walk's --config mount step takes its already-mounted branch and never
    # bind-mounts the booted root.
    env_text=$("$BTRDASD" config dump-env --config "$CONFIG_OUT") || env_text=
    walk_mount=$(
        eval "$env_text"
        printf '%s' "${DAS_TARGET_0_MOUNT:-}"
    ) || walk_mount=
    # The status is written only after the attempt: it never claims a
    # catch-up that did not start.
    if [[ -z "$walk_mount" ]]; then
        say "could not start the catch-up (the derived configuration names no target mount): search is current to $stamp only" >&2
        status "synced $stamp; catch-up NOT started: no target mount from dump-env"
    elif ! ensure_own_target "$ROOT$walk_mount"; then
        say "could not start the catch-up (cannot mount $walk_mount at its top level): search is current to $stamp only" >&2
        status "synced $stamp; catch-up NOT started: cannot mount $walk_mount"
    elif "$SYSTEMD_RUN" --quiet --collect --unit=das-recovery-state-catchup --property=Type=oneshot \
        "$BTRDASD" walk "$walk_mount" --db "$STATE_DIR/backup-index.db" --config "$CONFIG_OUT"; then
        status "synced $stamp; catch-up index running (data $data_stamp)"
    else
        say "could not start the catch-up (systemd-run failed): search is current to $stamp only" >&2
        status "synced $stamp; catch-up NOT started: systemd-run failed"
    fi
else
    status "synced $stamp"
fi
say "state synced from ${state_snap##*/}; configuration derived from ${root_snap##*/}"
