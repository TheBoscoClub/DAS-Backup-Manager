#!/bin/bash
# das-recovery-state-sync.sh — on a recovery drive's own OS, at boot: the
# newest received copy of btrdasd's state (index, records, history) made the
# live state at /var/lib/das-backup, and the host's config.toml, from the
# newest received root snapshot, derived into this OS's
# /etc/das-backup/config.toml (btrdasd config derive-recovery). Then, only when
# the state is older than the data (a night the state pass failed), an
# incremental index catch-up in the background.
# Version: 2.1.1
# Date: 2026-11-21
#
# The live state (bd DAS-Backup-Manager-eg4j.10) is a subvolume at the drive's
# top level, .das-recovery-state/current: each boot a writable snapshot of the
# newest received das-state (constant time, whatever the index's extent count;
# a file reflink of a 15 GB, 2.6M-extent index took 4-10 min on these drives),
# swapped in for the previous one, mounted at /var/lib/das-backup
# (nodev,nosuid,noexec) and read back before it is believed. It lives outside
# every OS root, so provision, undo, delete-replaced and the upgrade sessions'
# @ snapshots never meet it, and two levels down under a name that is not a
# btrbk "<name>.<stamp>", so the host's index walk of this target never reads
# it as a snapshot. The only subvolumes it deletes are .das-recovery-state/next
# and .das-recovery-state/previous, each only when btrfs says it is writable
# with no Received UUID (one this script made). A plain state directory from
# before 2.0.0 (and a .new/.old a crash left) is renamed aside into
# /var/lib/das-backup.migrating, never deleted on the boot path: after "ready",
# a background unit at idle I/O priority removes it (rm --one-file-system),
# and a boot that finds it still there starts the removal again.
#
# Where the host's snapshots are among what the backup runs received (the
# directory under the target, the root series' name, the state series' name) is
# not guessed: provisioning writes target_subdir=, root_series= and
# state_series= into the marker from the host's configuration (bd
# DAS-Backup-Manager-pzj6). A marker without one of them, or with one that is
# not a plain name, is a refusal like any other failure: the previous state
# stays, the journal and the status say which line is missing, and nothing
# falls back to a built-in name. The derived config.toml cannot carry them
# (it has no sources, and on a first boot not yet a file).
#
# Never runs on the host: ConditionPathExists=/etc/das-backup/recovery-drive
# in the unit, and the marker is written only by provisioning. A failure
# leaves the previous state in place — the previous boot's live state mounted
# again, or a not yet migrated plain directory where it was — and exits 1 with
# the reason in the journal and in $STATUS_FILE. Exit 2 (the unit lists it in
# SuccessExitStatus) is "no state snapshot yet": a normal first boot before
# any backup, not a failure. Run a second time in one boot (the state already
# mounted) it refuses and changes nothing.
#
# The catch-up walks the drive's OWN target, so that target is mounted here at
# subvolid=5 (its top level, by the booted UUID) before walk starts: walk then
# finds it mounted and never binds the running `/` (subvolume @) at its path.
# D9: the boot says what it is doing. After the marker gate the unit shows a
# fixed header (what is happening and why, "do not power off") plus one line per
# phase on the plymouth splash (plymouth display-message), only when plymouth
# answers --ping. Each phase's seconds and the total are measured into
# $TIMINGS (outside the state directory, which is swapped every boot), only on
# a boot that reached "ready"; the next boot shows a countdown from that
# measurement and from nothing else (no measurement: no countdown). On EVERY
# boot that passes the marker gate, whatever the sync then does (including the
# normal first boot with no state snapshot, exit 2), $BOOTED is written with
# one line, provisioned=<the marker's provisioned value> (empty when the marker
# has none): this root has booted, which is what recovery-os-install.sh undo
# refuses on and what delete-replaced compares with the newest manifest's
# stamp. It lives beside the timings file, outside the state directory
# (swapped every boot).
#
# It is left mounted for the rest of the recovery session (the GUI restore
# reads it); it is deliberately not in the EXIT trap.
set -euo pipefail

ROOT="${DAS_STATE_SYNC_ROOT:-}" # test seam: a fake filesystem tree
STATE_DIR="${DAS_STATE_SYNC_STATE_DIR:-$ROOT/var/lib/das-backup}"
CONFIG_OUT="${DAS_STATE_SYNC_CONFIG_OUT:-$ROOT/etc/das-backup/config.toml}"
# The siblings, guarded where they are made: an empty STATE_DIR or CONFIG_OUT aborts here
# instead of leaving a bare ".new", ".old" or ".migrating" relative to the cwd.
# .new/.old are only what a crash of a version before 2.0.0 left: set aside, never written.
STATE_NEW="${STATE_DIR:?}.new"
STATE_OLD="${STATE_DIR:?}.old"
MIGRATING="${STATE_DIR:?}.migrating"
CONFIG_NEW="${CONFIG_OUT:?}.new"
# Test seam like every DAS_STATE_SYNC_* name here: the unit never sets it, the
# suite does (bd eg4j.15, note 6); the unit runs as root with a fixed environment.
MARKER="${DAS_STATE_SYNC_MARKER:-$ROOT/etc/das-backup/recovery-drive}"
RUN_DIR="${DAS_STATE_SYNC_RUN_DIR:-$ROOT/run/das-recovery-state}"
TOP="${DAS_STATE_SYNC_TOP:-$RUN_DIR/top}" # the booted fs mounted at subvolid=5
BTRDASD="${DAS_STATE_SYNC_BTRDASD:-/usr/bin/btrdasd}"
# This script, by absolute path: the background removal runs it again.
SELF="${DAS_STATE_SYNC_SELF:-$(realpath -- "${BASH_SOURCE[0]}")}"
# Every external tool is a named seam (default: the bare name, found on PATH).
FINDMNT="${DAS_STATE_SYNC_FINDMNT:-findmnt}"
SYSTEMD_RUN="${DAS_STATE_SYNC_SYSTEMD_RUN:-systemd-run}"
MOUNT="${DAS_STATE_SYNC_MOUNT:-mount}"
UMOUNT="${DAS_STATE_SYNC_UMOUNT:-umount}"
BTRFS="${DAS_STATE_SYNC_BTRFS:-btrfs}"
MV="${DAS_STATE_SYNC_MV:-mv}"
PLYMOUTH="${DAS_STATE_SYNC_PLYMOUTH:-plymouth}"
TIMINGS="${DAS_STATE_SYNC_TIMINGS:-$ROOT/var/lib/das-recovery/timings}"
BAY="${DAS_STATE_SYNC_BAY:-}"
BOOTED="${DAS_STATE_SYNC_BOOTED:-$ROOT/var/lib/das-recovery/booted}"
STATUS_FILE="$RUN_DIR/status"
# The live state: fixed names, never computed from anything read off the disk.
LIVE_REL=".das-recovery-state"
LIVE_DIR="$TOP/$LIVE_REL"
LIVE="$LIVE_DIR/current"
LIVE_NEXT="$LIVE_DIR/next"
LIVE_PREV="$LIVE_DIR/previous"
LIVE_FSROOT="/$LIVE_REL/current" # what findmnt must read at the state directory
HEADER_TEXT='Recovery disk boots take longer than a normal boot: this disk is a 2 TB USB drive, and it prepares the latest backup index and settings before the desktop appears. Do not power off or unplug it while it does this.'
T0=$SECONDS
PREV_TOTAL=""
SPLASH=""
DYING=""
TOP_OK=""        # the top level is mounted and read back as this drive's
STATE_MOUNTED="" # the live state is mounted at $STATE_DIR and read back
declare -A PHASE_AT=()
SUBDIR=""        # the received directory, from the marker's target_subdir=
ROOT_SERIES=""   # the root snapshots' name, from root_series=
STATE_SERIES=""  # the state snapshots' name, from state_series=
# Spelled out, never a bracket range: a range follows the locale's collation.
ASCII_ALNUM=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789

say() { printf '%s\n' "$*"; }
status() {
    mkdir -p "$RUN_DIR"
    printf '%s\n' "$*" >"$STATUS_FILE"
}
# die <reason>: the reason goes to stderr (journal) and the status file; the
# previous live state is mounted again where it can be (restore_previous).
die() {
    say "das-recovery-state-sync: $*" >&2
    restore_previous
    [[ -n "$DYING" ]] || { DYING=1; phase "done" "could not update: $*"; }
    status "failed: $*"
    exit 1
}
# splash_init: once, after the marker gate. plymouth is spoken to only when it
# answers --ping (bare metal without a splash costs one failed ping).
splash_init() {
    [[ -n "$BAY" ]] || BAY=$(sed -n 's/^bay=//p' "$MARKER" 2>/dev/null | head -1) || :
    [[ -n "$BAY" ]] || {
        BAY="?"
        say "das-recovery-state-sync: the marker has no bay= line: the splash says Bay ?" >&2
    }
    if "$PLYMOUTH" --ping 2>/dev/null; then SPLASH=1; fi
    PREV_TOTAL=$(sed -n 's/^total=//p' "$TIMINGS" 2>/dev/null | head -1) || :
    [[ "$PREV_TOTAL" =~ ^[123456789][[:digit:]]{0,5}$ ]] || PREV_TOTAL=""
}
# phase <key> <text>: records when the phase began and shows it. A countdown
# exists only when the previous boot measured a total (a sane one: 1..999999),
# only on the working phases, and only while time is left.
phase() {
    local key=$1 text=$2 left msg
    PHASE_AT[$key]=$((SECONDS - T0))
    msg="DAS recovery disk, Bay $BAY. $HEADER_TEXT"$'\n\n'"$text"
    if [[ -n "$PREV_TOTAL" && "$key" != "done" ]]; then
        left=$((PREV_TOTAL - (SECONDS - T0)))
        ((left <= 0)) || msg+=" — about $left s remaining, measured at the last boot"
    fi
    say "das-recovery-state-sync: $text"
    [[ -z "$SPLASH" ]] || "$PLYMOUTH" display-message --text="$msg" 2>/dev/null || :
}
# measure: each phase's seconds and the total, replacing the previous
# measurement; called only on a boot that reached "ready".
measure() {
    local now=$((SECONDS - T0)) out="" keys=(find config copy) i k next
    for ((i = 0; i < ${#keys[@]}; i++)); do
        k=${keys[$i]}
        [[ -n "${PHASE_AT[$k]:-}" ]] || continue
        next=${keys[$((i + 1))]:-}
        if [[ -n "$next" && -n "${PHASE_AT[$next]:-}" ]]; then
            out+="$k=$((PHASE_AT[$next] - PHASE_AT[$k]))"$'\n'
        else
            out+="$k=$((now - PHASE_AT[$k]))"$'\n'
        fi
    done
    out+="total=$now"$'\n'
    if ! { mkdir -p "$(dirname "$TIMINGS")" && printf '%s' "$out" >"$TIMINGS.new" && "$MV" -f "$TIMINGS.new" "$TIMINGS"; }; then
        say "das-recovery-state-sync: could not write the boot measurement to $TIMINGS" >&2
    fi
}
cleanup_new() { rm -f "${CONFIG_NEW:?}"; }

# ours <path>: a real directory that btrfs reads as a WRITABLE subvolume with
# no Received UUID: a snapshot this script made. Anything else (read-only, a
# received copy, unreadable, a symlink) is not ours, and is never deleted.
ours() {
    local info
    [[ -d "$1" && ! -L "$1" ]] || return 1
    info=$("$BTRFS" subvolume show "$1" 2>/dev/null) || return 1
    ! grep -Eq '^[[:space:]]*Flags:.*readonly' <<<"$info" || return 1
    awk '$1 == "Received" && $2 == "UUID:" { seen = 1; plain = ($3 == "-") } END { exit !(seen && plain) }' <<<"$info"
}
# has_entries <dir>: holds anything at all (dot files included).
has_entries() {
    local e
    for e in "$1"/* "$1"/.[!.]* "$1"/..?*; do
        [[ -e "$e" || -L "$e" ]] && return 0
    done
    return 1
}
# mounted_as <mountpoint>: "UUID FSROOT" of what is mounted there; empty and
# nonzero when nothing is.
mounted_as() { "$FINDMNT" -no UUID,FSROOT -M "$1" 2>/dev/null; }
# mount_live: .das-recovery-state/current at $STATE_DIR, read back as this
# drive's and that subvolume before it is believed; a wrong mount is taken
# down again. Sets STATE_MOUNTED.
mount_live() {
    local seen u r
    "$MOUNT" -o "subvol=$LIVE_FSROOT,nodev,nosuid,noexec" "$src" "$STATE_DIR" || return 1
    seen=$(mounted_as "$STATE_DIR") || seen=""
    read -r u r <<<"$seen"
    if [[ "${u,,}" == "${uuid,,}" && "$r" == "$LIVE_FSROOT" ]]; then
        STATE_MOUNTED=1
        return 0
    fi
    say "das-recovery-state-sync: $STATE_DIR mounted as uuid ${u:-unknown}, fsroot ${r:-unknown}, not $LIVE_FSROOT: unmounted again" >&2
    "$UMOUNT" "$STATE_DIR" ||
        say "das-recovery-state-sync: could not unmount the wrong mount at $STATE_DIR" >&2
    return 1
}
# restore_previous: on a failure, the previous boot's live state mounted again,
# so the session never starts on an empty state directory. Only once the top
# level is verified, only when nothing is mounted there yet, only when the
# state directory is empty (an unmigrated plain copy IS the previous state),
# and only a subvolume this script made.
restore_previous() {
    [[ -n "$TOP_OK" && -z "$STATE_MOUNTED" ]] || return 0
    ours "$LIVE" || return 0
    if [[ -d "$STATE_DIR" ]]; then
        ! has_entries "$STATE_DIR" || return 0
    else
        mkdir -p "$STATE_DIR" || return 0
    fi
    if mount_live; then
        say "das-recovery-state-sync: the live state in $LIVE_REL/current (whichever this run left there) is mounted at $STATE_DIR" >&2
    else
        say "das-recovery-state-sync: could not mount the previous live state: $STATE_DIR is empty" >&2
    fi
}
# delete_ours <path>: one fixed path, never recursive, only if ours.
delete_ours() {
    ours "$1" || {
        say "das-recovery-state-sync: ${1##*/} is not a writable snapshot this script made: not deleted" >&2
        return 1
    }
    "$BTRFS" subvolume delete "$1" >/dev/null
}

# The marker gate writes nothing at all, not even a status file: on the host
# this must be a pure refusal.
if [[ ! -e "$MARKER" ]]; then
    say "das-recovery-state-sync: not a recovery drive ($MARKER absent): nothing done" >&2
    exit 1
fi

# --remove-set-aside: the background removal started after "ready" (below).
# One fixed path, on its own filesystem only (never into another subvolume
# or a mount), behind the same marker gate.
if [[ "${1:-}" == --remove-set-aside ]]; then
    if [[ -L "$MIGRATING" || ! -d "$MIGRATING" ]]; then
        say "das-recovery-state-sync: nothing to remove ($MIGRATING is not a directory)"
        exit 0
    fi
    rm -rf --one-file-system -- "${MIGRATING:?}" || {
        say "das-recovery-state-sync: removing $MIGRATING did not finish: the next boot starts it again" >&2
        exit 1
    }
    say "das-recovery-state-sync: removed the set-aside plain state $MIGRATING"
    exit 0
fi

prov=$(sed -n 's/^provisioned=//p' "$MARKER" 2>/dev/null | head -1) || prov=""
if ! { mkdir -p "$(dirname "$BOOTED")" && printf 'provisioned=%s\n' "$prov" >"$BOOTED"; }; then
    say "das-recovery-state-sync: could not write $BOOTED: an undo of the provisioning would not know this root has booted" >&2
fi

splash_init

uuid=$("$FINDMNT" -no UUID "${ROOT:-/}") || die "cannot read the booted filesystem's UUID"
[[ -n "$uuid" ]] || die "the booted filesystem has no UUID"
src=$("$FINDMNT" -no SOURCE "${ROOT:-/}") || die "cannot read the booted filesystem's source device"
src=${src%%\[*}
mkdir -p "$TOP"
# Read-write: the live state's snapshot is made here. So it is read back as
# this drive's top level before anything is written under it.
if [[ -z "$ROOT" ]]; then
    "$MOUNT" -o subvolid=5 "$src" "$TOP" || die "cannot mount the booted filesystem's top level"
    trap '"$UMOUNT" "$TOP" 2>/dev/null || :' EXIT
fi
top_seen=$(mounted_as "$TOP") || top_seen=""
read -r top_u top_r <<<"$top_seen"
[[ "${top_u,,}" == "${uuid,,}" && "$top_r" == "/" ]] ||
    die "$TOP is not this drive's top level (uuid ${top_u:-unknown}, fsroot ${top_r:-unknown}): nothing written"
TOP_OK=1

# Run a second time in one boot: the live state is in use there. An exchange
# under a mounted subvolume would carry the mount along with it.
# Not die(): nothing is touched, and this boot's status (from the first run)
# stays what it was.
if state_seen=$(mounted_as "$STATE_DIR") && [[ -n "$state_seen" ]]; then
    say "das-recovery-state-sync: $STATE_DIR is already mounted ($state_seen): the state was synced at this boot; reboot to sync again. Nothing changed." >&2
    exit 1
fi

# marker_series: the three names the marker carries, read from the marker only
# (never from the environment), each checked before any path is built from it.
# One plain name, or for the directory a relative path of them: never empty,
# never . or .., never absolute.
marker_series() {
    local k v
    for k in target_subdir root_series state_series; do
        v=$(sed -n "s/^$k=//p" "$MARKER" 2>/dev/null | head -1) || v=""
        [[ -n "$v" ]] ||
            die "the marker $MARKER has no $k= line (provisioning writes it from the host's configuration; this drive was provisioned without it): cannot tell where the backups are, state left as it was"
        [[ "$v" =~ ^[${ASCII_ALNUM}._-]+(/[${ASCII_ALNUM}._-]+)*$ && ! "$v" =~ (^|/)\.\.?(/|$) ]] ||
            die "the marker's $k= '$v' is not a plain name: state left as it was"
        [[ "$k" == target_subdir || "$v" != */* ]] ||
            die "the marker's $k= '$v' holds a '/': state left as it was"
        case "$k" in
            target_subdir) SUBDIR=$v ;;
            root_series) ROOT_SERIES=$v ;;
            *) STATE_SERIES=$v ;;
        esac
    done
}
marker_series

# The drive's own received directory for the host's root volume: the
# top-level directory the marker names.
# Every component is checked, not only the last: a link in the middle of a
# multi-part name would lead out of the drive's own tree (bd eg4j.15).
dir="$TOP"
IFS=/ read -ra subdir_parts <<<"${SUBDIR:?}"
for part in "${subdir_parts[@]}"; do
    dir="$dir/${part:?}"
    [[ -d "$dir" && ! -L "$dir" ]] || die "no received '$SUBDIR' directory on this filesystem: not a target that receives the host's root"
done

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
# sort); incomplete candidates are skipped with a logged line. Only
# "<prefix>.<btrbk stamp>" counts: YYYYMMDDTHHMM, with optional seconds and a
# _N suffix. A series "foo" never takes "foo.bar.<stamp>" (another series) or
# "foo.<stamp>.bak" (bd eg4j.15).
newest() {
    local g=("$dir/$1".*) c=() i stamp
    for i in "${g[@]}"; do
        stamp=${i##*/}
        stamp=${stamp#"$1".}
        [[ -e "$i" && "$stamp" =~ ^[0-9]{8}T[0-9]{4}([0-9]{2})?(_[0-9]+)?$ ]] && c+=("$i")
    done
    [[ ${#c[@]} -gt 0 ]] || return 0
    mapfile -t c < <(printf '%s\n' "${c[@]}" | LC_ALL=C sort -r)
    for i in "${c[@]}"; do
        if complete "$i"; then
            printf '%s\n' "$i"
            return 0
        fi
        say "das-recovery-state-sync: skipping ${i##*/}: not a completed receive (not read-only or no Received UUID)" >&2
    done
}

phase find "finding the newest backup"
state_snap=$(newest "${STATE_SERIES:?}")
root_snap=$(newest "${ROOT_SERIES:?}")

[[ -n "$root_snap" ]] || die "no received $ROOT_SERIES snapshot: the host's config.toml cannot be derived"
[[ -r "$root_snap/etc/das-backup/config.toml" ]] || die "the newest root snapshot ${root_snap##*/} has no etc/das-backup/config.toml"
if [[ -z "$state_snap" ]]; then
    # Not die(): the status word for this case is exactly "no state snapshot".
    say "das-recovery-state-sync: no received $STATE_SERIES snapshot yet (the next backup run sends one): state left as it was" >&2
    restore_previous
    phase "done" "no backup received yet"
    status "no state snapshot"
    exit 2
fi

# Config first: derive-recovery refuses before anything is replaced when the
# UUID is unknown or ambiguous.
phase config "preparing the configuration"
mkdir -p "$(dirname "$CONFIG_OUT")"
rm -f "${CONFIG_NEW:?}"
if ! "$BTRDASD" config derive-recovery --from "$root_snap/etc/das-backup/config.toml" \
    --booted-uuid "$uuid" --out "$CONFIG_OUT.new"; then
    cleanup_new
    die "derive-recovery refused (see above): state left as it was"
fi

# State: a writable snapshot of the received copy, made beside the live one
# and swapped in by rename; the live one is never half-replaced. Every step
# has its own failure branch: the previous live state stays (and is mounted
# again by die), the status says why.
phase copy "preparing the backup index"
if [[ -L "$LIVE_DIR" ]]; then
    cleanup_new
    die "$LIVE_DIR is a symlink: not followed, state left as it was"
fi
if [[ ! -d "$LIVE_DIR" ]]; then
    mkdir "$LIVE_DIR" || {
        cleanup_new
        die "cannot create $LIVE_DIR: state left as it was"
    }
fi
# What an interrupted boot left: next (snapshot made, not swapped in, or the
# swapped-out one not yet deleted) and previous (the same, without --exchange).
for left in "$LIVE_NEXT" "$LIVE_PREV"; do
    [[ -e "$left" || -L "$left" ]] || continue
    delete_ours "$left" || {
        cleanup_new
        die "${left##*/} was left by an earlier boot and could not be deleted: state left as it was"
    }
done
if ! "$BTRFS" subvolume snapshot "$state_snap" "$LIVE_NEXT" >/dev/null; then
    cleanup_new
    die "snapshotting ${state_snap##*/} failed: state left as it was"
fi
if ! ours "$LIVE_NEXT"; then
    cleanup_new
    die "the snapshot of ${state_snap##*/} does not read back as a writable snapshot without a Received UUID: state left as it was"
fi
replaced=""
if [[ ! -e "$LIVE" && ! -L "$LIVE" ]]; then
    "$MV" -T "$LIVE_NEXT" "$LIVE" || {
        cleanup_new
        die "installing the new state failed: state left as it was"
    }
elif ! ours "$LIVE"; then
    cleanup_new
    die "$LIVE is not a writable snapshot this script made: not replaced"
elif mv_help=$("$MV" --help 2>&1) && grep -q -- --exchange <<<"$mv_help"; then
    # One atomic step: current is never absent; next now holds the old one.
    "$MV" -T --exchange "$LIVE_NEXT" "$LIVE" || {
        cleanup_new
        die "swapping the new state in failed: state left as it was"
    }
    replaced=$LIVE_NEXT
else
    # No mv --exchange (coreutils < 9.5): two renames, undone if the second fails.
    "$MV" -T "$LIVE" "$LIVE_PREV" || {
        cleanup_new
        die "setting the old state aside failed: state left as it was"
    }
    "$MV" -T "$LIVE_NEXT" "$LIVE" || {
        cleanup_new
        "$MV" -T "$LIVE_PREV" "$LIVE" ||
            die "swap failed and the previous state could not be restored: it is in $LIVE_PREV"
        die "installing the new state failed: previous state restored"
    }
    replaced=$LIVE_PREV
fi

# The state directory itself is only a mountpoint now. A plain state from
# before 2.0.0 (and what a crash of one left beside it) is renamed aside, a
# constant-time rename on the same subvolume; its removal waits for "ready".
asides=()
set_aside() { # set_aside <path> <name>: into $MIGRATING/<name>.<epoch>
    local to
    if [[ -L "$MIGRATING" ]] || ! mkdir -p "$MIGRATING"; then return 1; fi
    to="$MIGRATING/$2.$(printf '%(%Y%m%dT%H%M%S)T' -1)"
    [[ ! -e "$to" && ! -L "$to" ]] || return 1
    "$MV" -T "$1" "$to" || return 1
    asides+=("$to")
}
for left in "$STATE_NEW" "$STATE_OLD"; do
    [[ -e "$left" || -L "$left" ]] || continue
    set_aside "$left" "${left##*.}" ||
        say "das-recovery-state-sync: could not set $left aside: left where it is" >&2
done
state_aside=""
if [[ -L "$STATE_DIR" ]]; then
    cleanup_new
    die "$STATE_DIR is a symlink: not followed, state left as it was"
elif [[ -d "$STATE_DIR" ]] && has_entries "$STATE_DIR"; then
    set_aside "$STATE_DIR" state || {
        cleanup_new
        die "could not set the plain state directory aside into $MIGRATING: state left as it was"
    }
    state_aside=${asides[-1]}
fi
if ! { mkdir -p "$STATE_DIR" && mount_live; }; then
    cleanup_new
    if [[ -n "$state_aside" ]]; then
        if rmdir "$STATE_DIR" 2>/dev/null && "$MV" -T "$state_aside" "$STATE_DIR"; then
            die "mounting the new state at $STATE_DIR failed: the plain state is back in place"
        fi
        die "mounting the new state at $STATE_DIR failed, and the plain state could not be put back: it is in $state_aside"
    fi
    die "mounting the new state at $STATE_DIR failed: state left as it was"
fi
"$MV" "$CONFIG_OUT.new" "$CONFIG_OUT" || {
    cleanup_new
    die "installing the derived configuration failed: state is new, configuration is the previous one"
}

# The installation's own recovery documents ([recovery_os].local_docs_dir),
# from the same root snapshot as the configuration (bd DAS-Backup-Manager-o77b):
# the host refreshes their lists before each backup's snapshots, so this copy
# is as current as the newest backup, offline. Each regular, plainly named *.md
# replaces this OS's copy (written beside it, then renamed); nothing is
# deleted, no symlink is followed. Documents never fail the sync: any problem
# is one line on stderr and the previous copies stay.
copy_local_docs() {
    # Spelled out, never a bracket range: a range follows the locale's collation.
    local ascii_alnum=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789
    local env_text rc dir src dest f n tmp copied=0
    env_text=$("$BTRDASD" config dump-env --config "$CONFIG_OUT" 2>/dev/null && printf '\nstatus=0' || printf '\nstatus=%s' "$?")
    rc=${env_text##*$'\n'status=}
    [[ "$env_text" == *$'\n'status=* && "$rc" == 0 ]] || {
        say "das-recovery-state-sync: local documents not copied: dump-env exited ${rc:-unknown}" >&2
        return 0
    }
    dir=$(sed -n "s/^DAS_RECOVERY_OS_LOCAL_DOCS_DIR='\(.*\)'\$/\1/p" <<<"$env_text")
    [[ -n "$dir" ]] || return 0
    # As config.rs local_docs_dir_is_valid: absolute, plain, no . or .. component.
    [[ "$dir" =~ ^(/[${ascii_alnum}._-]+)+$ && ! "$dir" =~ (^|/)\.\.?(/|$) ]] || {
        say "das-recovery-state-sync: local documents not copied: '$dir' is not a plain absolute path" >&2
        return 0
    }
    src="$root_snap$dir"
    dest="$ROOT$dir"
    [[ -d "$src" && ! -L "$src" ]] || return 0
    mkdir -p "$dest" || {
        say "das-recovery-state-sync: local documents not copied: cannot create $dest" >&2
        return 0
    }
    for f in "$src"/*.md; do
        n=${f##*/}
        [[ -f "$f" && ! -L "$f" && "$n" =~ ^[${ascii_alnum}._-]+$ ]] || continue
        tmp="$dest/.$n.new"
        # Whatever a crash left there, a symlink included, is removed first:
        # cp would write through a link.
        rm -f -- "${tmp:?}"
        if cp -- "$f" "$tmp" &&"$MV" -f -- "$tmp" "$dest/$n"; then
            copied=$((copied + 1))
        else
            rm -f -- "${tmp:?}"
            say "das-recovery-state-sync: local document $n not copied: the previous copy stays" >&2
        fi
    done
    say "local documents: $copied copied from ${root_snap##*/}"
}
copy_local_docs

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
measure
phase "done" "ready: index from $stamp"
say "state synced from ${state_snap##*/}; configuration derived from ${root_snap##*/}"

# After "ready", off the desktop's path. The replaced snapshot: one ioctl, the
# kernel's cleaner drops its extents later; a failure leaves it for the next
# boot to delete. A set-aside plain state: its removal is the slow part (each
# reflinked extent dropped), so a background unit at idle I/O priority runs
# this script's --remove-set-aside; a boot that finds it still there starts it again.
if [[ -n "$replaced" ]]; then
    delete_ours "$replaced" ||
        say "das-recovery-state-sync: could not delete the replaced state ${replaced##*/}: the next boot deletes it" >&2
fi
if [[ -d "$MIGRATING" && ! -L "$MIGRATING" ]]; then
    if "$SYSTEMD_RUN" --quiet --collect --unit=das-recovery-state-cleanup --property=Type=oneshot \
        --property=IOSchedulingClass=idle --property=Nice=19 "$SELF" --remove-set-aside; then
        say "removing the set-aside plain state ($MIGRATING) in the background"
    else
        say "das-recovery-state-sync: could not start the removal of $MIGRATING: the next boot tries again" >&2
    fi
fi
