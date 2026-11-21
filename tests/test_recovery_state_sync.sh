#!/bin/bash
#
# tests/test_recovery_state_sync.sh
#
# scripts/das-recovery-state-sync.sh against a fixture tree and stub findmnt,
# btrdasd, systemd-run, mount, umount, btrfs and mv. Runs as an ordinary user;
# nothing is mounted. Every case starts from a fixture reset, and every tool
# the script calls is reached through its DAS_STATE_SYNC_* seam.
set -euo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/das-recovery-state-sync.sh"
WORK=$(mktemp -d)
trap 'chmod -R u+rwX "$WORK" 2>/dev/null || :; rm -rf "${WORK:?}"' EXIT
R="$WORK/root"
BIN="$WORK/bin"
LOG="$WORK/calls.log"
UUID_STUB="1111-aaaa-stub-uuid"
WALK_MOUNT="/mnt/backup-system-recovery-A"
FAILS=0

pass() { printf 'ok   %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; FAILS=$((FAILS + 1)); }
check() { # check <name> <command...>
    local name=$1
    shift
    if "$@"; then pass "$name"; else fail "$name"; fi
}
has() { grep -qF -- "$2" "$1" 2>/dev/null; }
file_lacks() { ! grep -qF -- "$2" "$1" 2>/dev/null; }
out_has() { grep -qF -- "$1" <<<"$OUT"; }
status_is() { [[ "$(cat "$R/run/das-recovery-state/status" 2>/dev/null)" == "$1" ]]; }
no_leftovers() { test ! -e "$R/.das-recovery-state/next" -a ! -e "$R/.das-recovery-state/previous" -a ! -e "$R/etc/das-backup/config.toml.new"; }
no_writes() { file_lacks "$LOG" "subvolume snapshot" && file_lacks "$LOG" "subvolume delete"; }
no_walk() { file_lacks "$LOG" " walk "; } # the catch-up (systemd-run also starts the set-aside removal)

mkdir -p "$BIN"
# findmnt -no UUID,FSROOT -M <mp>: the own target's mount state. STUB_MNT (a
# full "UUID FSROOT" line) forces it already mounted as that; otherwise it is
# mounted (as this drive's top level) only after the mount stub ran for it.
# The top level (\$R under the TOP seam) and the state directory answer for
# themselves: STUB_TOP_MNT, and STUB_STATE_PREMOUNTED / STUB_STATE_MNT (the
# state directory reads mounted only after the mount stub mounted a subvol= there).
cat >"$BIN/findmnt" <<S
#!/bin/bash
last=\${!#}
case "\$*" in
*FSROOT*)
    if [[ "\$last" == "$R" ]]; then echo "\${STUB_TOP_MNT:-$UUID_STUB /}"
    elif [[ "\$last" == "$R/var/lib/das-backup" ]]; then
        if [[ -n "\${STUB_STATE_PREMOUNTED:-}" ]]; then echo "\$STUB_STATE_PREMOUNTED"
        elif [[ -e "\$STUB_LOG.state-mounted" ]]; then echo "\${STUB_STATE_MNT:-$UUID_STUB /.das-recovery-state/current}"
        else exit 1; fi
    elif [[ -n "\${STUB_MNT:-}" ]]; then echo "\$STUB_MNT"
    elif [[ -e "\$STUB_LOG.mounted" ]]; then echo "$UUID_STUB /"
    else exit 1; fi ;;
*UUID*) echo "$UUID_STUB" ;;
*SOURCE*) echo "/dev/stub[/@]" ;;
esac
S
cat >"$BIN/btrdasd" <<S
#!/bin/bash
echo "btrdasd \$*" >>"\$STUB_LOG"
if [[ "\$1 \$2" == "config derive-recovery" ]]; then
    if [[ -n "\${STUB_DERIVE_FAIL:-}" ]]; then
        echo "derive-recovery: refused" >&2
        exit 1
    fi
    out=
    while ((\$#)); do
        [[ "\$1" == --out ]] && out=\$2
        shift
    done
    echo "recovery_drive = true" >"\$out"
elif [[ "\$1 \$2" == "config dump-env" && -z "\${STUB_DUMPENV_EMPTY:-}" ]]; then
    echo "DAS_TARGET_COUNT=2"
    echo "DAS_TARGET_0_MOUNT='$WALK_MOUNT'"
    [[ -z "\${STUB_DOCS_DIR:-}" ]] || echo "DAS_RECOVERY_OS_LOCAL_DOCS_DIR='\$STUB_DOCS_DIR'"
fi
S
cat >"$BIN/systemd-run" <<'S'
#!/bin/bash
echo "systemd-run $*" >>"$STUB_LOG"
[[ -z "${STUB_SYSTEMD_RUN_FAIL:-}" ]]
S
cat >"$BIN/mount" <<'S'
#!/bin/bash
echo "mount $*" >>"$STUB_LOG"
if [[ "$*" == *subvol=/* ]]; then
    [[ -z "${STUB_STATE_MOUNT_FAIL:-}" ]] || exit 32
    : >"$STUB_LOG.state-mounted"
    exit 0
fi
[[ -z "${STUB_MOUNT_FAIL:-}" ]] || exit 32
[[ "$*" != *UUID=* ]] || : >"$STUB_LOG.mounted"
S
cat >"$BIN/umount" <<'S'
#!/bin/bash
echo "umount $*" >>"$STUB_LOG"
[[ "${!#}" != */var/lib/das-backup ]] || rm -f "${STUB_LOG:?}.state-mounted"
S
# btrfs subvolume snapshot <src> <dst>: a copy (STUB_SNAPSHOT_FAIL fails it);
# subvolume delete <path>: removes it (STUB_DELETE_FAIL fails it).
# subvolume show <path>: under .das-recovery-state, a writable snapshot with no
# Received UUID unless the path contains $STUB_LIVE_FOREIGN (then read-only and
# received: not one this script made); elsewhere complete (read-only + Received
# UUID) unless the path contains $STUB_PARTIAL (an interrupted receive).
cat >"$BIN/btrfs" <<'S'
#!/bin/bash
echo "btrfs $*" >>"$STUB_LOG"
case "$1 $2" in
"subvolume snapshot")
    [[ -z "${STUB_SNAPSHOT_FAIL:-}" ]] || { echo "stub: snapshot failed" >&2; exit 1; }
    exec /usr/bin/cp -a -- "$3" "$4" ;;
"subvolume delete")
    [[ -z "${STUB_DELETE_FAIL:-}" ]] || { echo "stub: delete failed" >&2; exit 1; }
    p=$3
    exec /usr/bin/rm -rf -- "${p:?}" ;;
esac
if [[ "$3" == */.das-recovery-state/* ]]; then
    if [[ -n "${STUB_LIVE_FOREIGN:-}" && "$3" == *"$STUB_LIVE_FOREIGN"* ]]; then
        printf '%s\n' "$3" "	Received UUID: 		aaaaaaaa-0000-1111-2222-333333333333" "	Flags: 			readonly"
    else
        printf '%s\n' "$3" "	Received UUID: 		-" "	Flags: 			-"
    fi
    exit 0
fi
if [[ -n "${STUB_PARTIAL:-}" && "$3" == *"$STUB_PARTIAL"* ]]; then
    printf '%s\n' "$3" "	Received UUID: 		-" "	Flags: 			-"
else
    printf '%s\n' "$3" "	Received UUID: 		aaaaaaaa-0000-1111-2222-333333333333" "	Flags: 			readonly"
fi
S
# mv: passes through to the real one. STUB_MV_FAIL_AT="1 3" fails the 1st and
# 3rd real call (counted in $STUB_MV_COUNT; --help is not counted).
# STUB_MV_NO_EXCHANGE=1 hides --exchange from --help, so the script takes its
# two-step fallback.
cat >"$BIN/mv" <<'S'
#!/bin/bash
if [[ "${1:-}" == --help ]]; then
    if [[ -n "${STUB_MV_NO_EXCHANGE:-}" ]]; then /usr/bin/mv --help | grep -v -- --exchange; else /usr/bin/mv --help; fi
    exit 0
fi
if [[ -n "${STUB_MV_FAIL_AT:-}" ]]; then
    n=$(($(cat "$STUB_MV_COUNT" 2>/dev/null || echo 0) + 1))
    echo "$n" >"$STUB_MV_COUNT"
    for f in $STUB_MV_FAIL_AT; do
        if [[ "$f" == "$n" ]]; then
            echo "mv: stub failure (call $n)" >&2
            exit 1
        fi
    done
fi
exec /usr/bin/mv "$@"
S
# plymouth: logs every call; --ping fails when STUB_PLYMOUTH_DOWN is set.
cat >"$BIN/plymouth" <<'S'
#!/bin/bash
echo "plymouth $*" >>"$STUB_LOG"
[[ -z "${STUB_PLYMOUTH_SLOW:-}" || "${1:-}" != display-message ]] || sleep "$STUB_PLYMOUTH_SLOW"
[[ "${1:-}" != --ping || -z "${STUB_PLYMOUTH_DOWN:-}" ]]
S
chmod +x "$BIN"/*

fixture() {
    chmod -R u+rwX "$R" 2>/dev/null || :
    rm -rf "${R:?}"
    : >"$LOG"
    rm -f "${WORK:?}/mv-count" "${WORK:?}/calls.log.mounted" "${WORK:?}/calls.log.state-mounted" "${WORK:?}/timings" "${WORK:?}/booted"
    mkdir -p "$R/etc/das-backup" "$R/run" "$R/var/lib/das-backup" \
        "$R/nvme/root-.20261012T0348/etc/das-backup" "$R/nvme/das-state.20261012T0348"
    printf 'label=system-recovery-A-2tb\nbay=1\nprovisioned=20261013-010203\ntarget_subdir=nvme\nroot_series=root-\nstate_series=das-state\n' >"$R/etc/das-backup/recovery-drive"
    echo previous >"$R/var/lib/das-backup/previous"
    echo "# host config" >"$R/nvme/root-.20261012T0348/etc/das-backup/config.toml"
    echo db-0348 >"$R/nvme/das-state.20261012T0348/backup-index.db"
}

RC=0
OUT=
run() { # run [VAR=value ...]: the script under the seams; RC and OUT hold the result
    RC=0
    OUT=$(env PATH="$BIN:$PATH" STUB_LOG="$LOG" STUB_MV_COUNT="$WORK/mv-count" DAS_STATE_SYNC_ROOT="$R" DAS_STATE_SYNC_TOP="$R" \
        DAS_STATE_SYNC_BTRDASD="$BIN/btrdasd" DAS_STATE_SYNC_FINDMNT="$BIN/findmnt" \
        DAS_STATE_SYNC_SYSTEMD_RUN="$BIN/systemd-run" DAS_STATE_SYNC_MOUNT="$BIN/mount" \
        DAS_STATE_SYNC_UMOUNT="$BIN/umount" DAS_STATE_SYNC_BTRFS="$BIN/btrfs" \
        DAS_STATE_SYNC_MV="$BIN/mv" DAS_STATE_SYNC_PLYMOUTH="$BIN/plymouth" \
        DAS_STATE_SYNC_TIMINGS="$WORK/timings" DAS_STATE_SYNC_BOOTED="$WORK/booted" "$@" bash "$SCRIPT" 2>&1) || RC=$?
    [[ -z "${SHOW_OUT:-}" ]] || printf '%s\n' "$OUT"
}

LIVE="$R/.das-recovery-state/current"
echo "== live state: a writable snapshot at the top level, mounted (bd eg4j.10)"
fixture
run
check "live: exit 0" test "$RC" -eq 0
check "live: the snapshot holds the newest state" has "$LIVE/backup-index.db" db-0348
check "live: made by btrfs subvolume snapshot, not a file copy" has "$LOG" "btrfs subvolume snapshot $R/nvme/das-state.20261012T0348 $R/.das-recovery-state/next"
check "live: mounted at the state directory, nodev,nosuid,noexec" has "$LOG" "mount -o subvol=/.das-recovery-state/current,nodev,nosuid,noexec"
check "live: nothing copied into the state directory itself" test ! -e "$R/var/lib/das-backup/backup-index.db"
check "live: the old plain state set aside, not deleted inline" test -n "$(find "$R/var/lib/das-backup.migrating" -name previous)"
check "live: its removal started in the background at idle I/O" has "$LOG" "--property=IOSchedulingClass=idle"

echo "== synced"
fixture
run
check "synced: exit 0" test "$RC" -eq 0
check "synced: index in the live state" has "$LIVE/backup-index.db" db-0348
check "synced: previous state replaced" test ! -e "$R/var/lib/das-backup/previous"
check "synced: config derived" has "$R/etc/das-backup/config.toml" "recovery_drive = true"
check "synced: status" status_is "synced 20261012T0348"
check "synced: derive-recovery args" has "$LOG" "--booted-uuid $UUID_STUB"
check "synced: derive-recovery source" has "$LOG" "--from $R/nvme/root-.20261012T0348/etc/das-backup/config.toml"
check "synced: no catch-up" no_walk
check "synced: no top-level mount under the ROOT seam" file_lacks "$LOG" "mount -o subvolid=5 "
check "synced: no leftovers" no_leftovers

echo "== gap (state older than data)"
fixture
mkdir -p "$R/nvme/root-.20261013T0348/etc/das-backup"
echo "# newer" >"$R/nvme/root-.20261013T0348/etc/das-backup/config.toml"
run
check "gap: exit 0" test "$RC" -eq 0
check "gap: older state is live" has "$LIVE/backup-index.db" db-0348
check "gap: status" status_is "synced 20261012T0348; catch-up index running (data 20261013T0348)"
check "gap: config from the newest root" has "$LOG" "--from $R/nvme/root-.20261013T0348/etc/das-backup/config.toml"
check "gap: catch-up via systemd-run" has "$LOG" "systemd-run"
check "gap: walk of the derived config's first target mount, --db, --config" \
    has "$LOG" "walk $WALK_MOUNT --db $R/var/lib/das-backup/backup-index.db --config $R/etc/das-backup/config.toml"
check "gap: walk is NOT given the top-level mount" file_lacks "$LOG" "walk $R"
check "gap: own target mounted at subvolid=5,ro by the booted uuid" \
    has "$LOG" "mount -o subvolid=5,ro UUID=$UUID_STUB $R$WALK_MOUNT"
check "gap: mount happens before the catch-up" \
    test "$(grep -n 'mount -o subvolid=5,ro UUID=' "$LOG" | head -1 | cut -d: -f1)" -lt "$(grep -n 'systemd-run' "$LOG" | head -1 | cut -d: -f1)"
check "gap: own target is NOT unmounted (left for the recovery session)" file_lacks "$LOG" "umount $R$WALK_MOUNT"
check "gap: mount point created" test -d "$R$WALK_MOUNT"

echo "== gap: own target already mounted"
fixture
mkdir -p "$R/nvme/root-.20261013T0348/etc/das-backup"
echo "# newer" >"$R/nvme/root-.20261013T0348/etc/das-backup/config.toml"
run STUB_MNT="$UUID_STUB /"
check "already top level: exit 0" test "$RC" -eq 0
check "already top level: not mounted again" file_lacks "$LOG" "mount -o subvolid=5,ro UUID="
check "already top level: catch-up starts" has "$LOG" "systemd-run"
fixture
mkdir -p "$R/nvme/root-.20261013T0348/etc/das-backup"
echo "# newer" >"$R/nvme/root-.20261013T0348/etc/das-backup/config.toml"
run STUB_MNT="$UUID_STUB /@"
check "bind of @: exit 1" test "$RC" -eq 1
check "bind of @: status" status_is "failed: $WALK_MOUNT is mounted but not this drive's top level (uuid $UUID_STUB, fsroot /@)"
check "bind of @: no catch-up" file_lacks "$LOG" systemd-run
fixture
mkdir -p "$R/nvme/root-.20261013T0348/etc/das-backup"
echo "# newer" >"$R/nvme/root-.20261013T0348/etc/das-backup/config.toml"
run STUB_MNT="OTHER-UUID /"
check "other filesystem: exit 1" test "$RC" -eq 1
check "other filesystem: no catch-up" file_lacks "$LOG" systemd-run
fixture
mkdir -p "$R/nvme/root-.20261013T0348/etc/das-backup"
echo "# newer" >"$R/nvme/root-.20261013T0348/etc/das-backup/config.toml"
run STUB_MOUNT_FAIL=1
check "mount fails: exit 0 (the sync itself succeeded)" test "$RC" -eq 0
check "mount fails: no catch-up" no_walk
check "mount fails: status says NOT started" status_is "synced 20261012T0348; catch-up NOT started: cannot mount $WALK_MOUNT"

echo "== state newer than data (the normal nightly layout)"
fixture
mkdir -p "$R/nvme/das-state.20261012T0412"
echo db-0412 >"$R/nvme/das-state.20261012T0412/backup-index.db"
run
check "newer: exit 0" test "$RC" -eq 0
check "newer: newest state is live" has "$LIVE/backup-index.db" db-0412
check "newer: no catch-up" no_walk
check "newer: status" status_is "synced 20261012T0412"

echo "== partial receive skipped"
fixture
mkdir -p "$R/nvme/das-state.20261012T0412"
echo db-partial >"$R/nvme/das-state.20261012T0412/backup-index.db"
run STUB_PARTIAL="das-state.20261012T0412"
check "partial: exit 0" test "$RC" -eq 0
check "partial: previous complete state used" has "$LIVE/backup-index.db" db-0348
check "partial: skip is logged" out_has "skipping das-state.20261012T0412"
check "partial: status names the complete one" status_is "synced 20261012T0348"

echo "== no state snapshot"
fixture
rm -rf "${R:?}"/nvme/das-state.*
run
check "nosnap: exit 2 (the unit's SuccessExitStatus), not 1" test "$RC" -eq 2
check "nosnap: the unit lists exit 2 as success" grep -q '^SuccessExitStatus=2$' "$(dirname "$SCRIPT")/../systemd/das-recovery-state-sync.service"
check "nosnap: previous state intact" has "$R/var/lib/das-backup/previous" previous
check "nosnap: no config written" test ! -e "$R/etc/das-backup/config.toml"
check "nosnap: status" status_is "no state snapshot"
check "nosnap: the booted marker is written on a boot that synced nothing" test "$(cat "$WORK/booted")" = "provisioned=20261013-010203"

echo "== no marker"
fixture
rm -f "${R:?}/etc/das-backup/recovery-drive" "${WORK:?}/booted"
run
check "nomarker: exit 1" test "$RC" -eq 1
check "nomarker: message" out_has "not a recovery drive"
check "nomarker: previous state intact" has "$R/var/lib/das-backup/previous" previous
check "nomarker: no config written" test ! -e "$R/etc/das-backup/config.toml"
check "nomarker: no status written" test ! -e "$R/run/das-recovery-state/status"
check "nomarker: no tools called" test ! -s "$LOG"
check "nomarker: no booted marker (the host is not a recovery boot)" test ! -e "$WORK/booted"

echo "== derive-recovery refuses"
fixture
run STUB_DERIVE_FAIL=1
check "derive: exit 1" test "$RC" -eq 1
check "derive: previous state intact" has "$R/var/lib/das-backup/previous" previous
check "derive: index not replaced" test ! -e "$R/var/lib/das-backup/backup-index.db"
check "derive: no config written" test ! -e "$R/etc/das-backup/config.toml"
check "derive: no leftovers" no_leftovers
check "derive: status" status_is "failed: derive-recovery refused (see above): state left as it was"

echo "== snapshot fails"
fixture
run STUB_SNAPSHOT_FAIL=1
check "snapshot: exit 1" test "$RC" -eq 1
check "snapshot: previous state intact" has "$R/var/lib/das-backup/previous" previous
check "snapshot: no config written" test ! -e "$R/etc/das-backup/config.toml"
check "snapshot: no leftovers" no_leftovers
check "snapshot: status" status_is "failed: snapshotting das-state.20261012T0348 failed: state left as it was"

echo "== swap (mv) fails"
fixture
run STUB_MV_FAIL_AT="1"
check "mv: exit 1" test "$RC" -eq 1
check "mv: previous state intact" has "$R/var/lib/das-backup/previous" previous
check "mv: index not installed" test ! -e "$R/var/lib/das-backup/backup-index.db"
check "mv: no config written" test ! -e "$R/etc/das-backup/config.toml"
check "mv: the unswapped snapshot is left for the next boot (deleted only through ours())" test -d "$R/.das-recovery-state/next"
check "mv: no config.new" test ! -e "$R/etc/das-backup/config.toml.new"
check "mv: status is a failure" grep -q '^failed: ' "$R/run/das-recovery-state/status"

# An earlier boot's live state, already migrated: the state directory is an
# empty mountpoint and current holds that boot's index.
prior_live() {
    rm -rf "${R:?}/var/lib/das-backup"
    mkdir -p "$R/var/lib/das-backup" "$LIVE"
    echo db-earlier >"$LIVE/backup-index.db"
}

echo "== second boot: the exchange, and the replaced one deleted after ready"
fixture; prior_live; run
check "exchange: exit 0" test "$RC" -eq 0
check "exchange: the new state is live" has "$LIVE/backup-index.db" db-0348
check "exchange: the replaced one deleted" has "$LOG" "btrfs subvolume delete $R/.das-recovery-state/next"
check "exchange: no leftovers" no_leftovers
check "exchange: nothing to set aside, no removal started" file_lacks "$LOG" "--remove-set-aside"
check "exchange: mounted" has "$LOG" "mount -o subvol=/.das-recovery-state/current,nodev,nosuid,noexec /dev/stub $R/var/lib/das-backup"

echo "== swap fallback (no mv --exchange)"
fixture; prior_live
run STUB_MV_NO_EXCHANGE=1
check "fallback: exit 0" test "$RC" -eq 0
check "fallback: state replaced" has "$LIVE/backup-index.db" db-0348
check "fallback: the old one deleted" has "$LOG" "btrfs subvolume delete $R/.das-recovery-state/previous"
check "fallback: no leftovers" no_leftovers
fixture; prior_live
run STUB_MV_NO_EXCHANGE=1 STUB_MV_FAIL_AT="2"
check "fallback 2nd move fails: exit 1" test "$RC" -eq 1
check "fallback 2nd move fails: previous state restored" has "$LIVE/backup-index.db" db-earlier
check "fallback 2nd move fails: and mounted again" has "$LOG" "mount -o subvol=/.das-recovery-state/current"
check "fallback 2nd move fails: status" status_is "failed: installing the new state failed: previous state restored"
fixture; prior_live
run STUB_MV_NO_EXCHANGE=1 STUB_MV_FAIL_AT="2 3"
check "fallback undo fails: exit 1" test "$RC" -eq 1
check "fallback undo fails: previous state kept in previous" has "$R/.das-recovery-state/previous/backup-index.db" db-earlier
check "fallback undo fails: never claims restored" file_lacks "$R/run/das-recovery-state/status" "previous state restored"
check "fallback undo fails: status says where it is" \
    status_is "failed: swap failed and the previous state could not be restored: it is in $R/.das-recovery-state/previous"
check "fallback undo fails: nothing deleted" file_lacks "$LOG" "subvolume delete"

echo "== a failure after the first boot mounts the previous live state again"
fixture; prior_live; run STUB_DERIVE_FAIL=1
check "derive after migration: exit 1" test "$RC" -eq 1
check "derive after migration: previous live state mounted" has "$LOG" "mount -o subvol=/.das-recovery-state/current,nodev,nosuid,noexec"
check "derive after migration: it is the earlier one" has "$LIVE/backup-index.db" db-earlier
check "derive after migration: no snapshot made" file_lacks "$LOG" "subvolume snapshot"
fixture; prior_live; rm -rf "${R:?}"/nvme/das-state.*; run
check "nosnap after migration: exit 2" test "$RC" -eq 2
check "nosnap after migration: previous live state mounted" has "$LOG" "mount -o subvol=/.das-recovery-state/current"
fixture; rm -rf "${R:?}"/nvme/das-state.* "${R:?}/var/lib/das-backup"; run
check "nosnap, never synced: nothing mounted (provision's first-boot check)" file_lacks "$LOG" "subvol="

echo "== the state mount fails or reads back wrong"
fixture; run STUB_STATE_MOUNT_FAIL=1
check "state mount fails: exit 1" test "$RC" -eq 1
check "state mount fails: the plain state is put back" has "$R/var/lib/das-backup/previous" previous
check "state mount fails: status" status_is "failed: mounting the new state at $R/var/lib/das-backup failed: the plain state is back in place"
check "state mount fails: config not installed" test ! -e "$R/etc/das-backup/config.toml"
fixture; run STUB_STATE_MNT="$UUID_STUB /@"
check "wrong state mount: exit 1" test "$RC" -eq 1
check "wrong state mount: taken down again" has "$LOG" "umount $R/var/lib/das-backup"
check "wrong state mount: the plain state is put back" has "$R/var/lib/das-backup/previous" previous

echo "== run twice in one boot: refused, nothing changed"
fixture; mkdir -p "$R/run/das-recovery-state"; echo "synced 20261012T0348" >"$R/run/das-recovery-state/status"
run STUB_STATE_PREMOUNTED="$UUID_STUB /.das-recovery-state/current"
check "twice: exit 1" test "$RC" -eq 1
check "twice: says so" out_has "already mounted"
check "twice: no snapshot, no mount, no delete" test -z "$(grep -E 'subvolume (snapshot|delete)|subvol=' "$LOG")"
check "twice: this boot's status kept" status_is "synced 20261012T0348"
check "twice: plain state untouched" has "$R/var/lib/das-backup/previous" previous

echo "== the top level is not this drive's"
fixture; run STUB_TOP_MNT="OTHER-UUID /"
check "top: exit 1" test "$RC" -eq 1
check "top: nothing written under it" test ! -e "$R/.das-recovery-state"
check "top: no snapshot" file_lacks "$LOG" "subvolume snapshot"
check "top: nothing mounted at the state directory" file_lacks "$LOG" "subvol="
check "top: status" status_is "failed: $R is not this drive's top level (uuid OTHER-UUID, fsroot /): nothing written"

echo "== only what this script made is ever deleted or replaced"
fixture; mkdir -p "$R/.das-recovery-state/next"; run STUB_LIVE_FOREIGN=/next
check "foreign next: exit 1" test "$RC" -eq 1
check "foreign next: not deleted" test -d "$R/.das-recovery-state/next"
check "foreign next: no delete called" file_lacks "$LOG" "subvolume delete"
fixture; mkdir -p "$R/.das-recovery-state/next"; echo stale >"$R/.das-recovery-state/next/x"; run
check "own leftover next: exit 0" test "$RC" -eq 0
check "own leftover next: deleted before the snapshot" \
    test "$(grep -n "subvolume delete $R/.das-recovery-state/next" "$LOG" | head -1 | cut -d: -f1)" -lt "$(grep -n 'subvolume snapshot' "$LOG" | head -1 | cut -d: -f1)"
check "own leftover next: synced" has "$LIVE/backup-index.db" db-0348
fixture; prior_live; run STUB_LIVE_FOREIGN=/current
check "foreign current: exit 1" test "$RC" -eq 1
check "foreign current: not replaced" has "$LIVE/backup-index.db" db-earlier
check "foreign current: never mounted (not ours)" file_lacks "$LOG" "subvol="
fixture; mkdir -p "$WORK/elsewhere"; ln -s "$WORK/elsewhere" "$R/.das-recovery-state"; run
check "symlinked container: exit 1" test "$RC" -eq 1
check "symlinked container: not followed" test -z "$(ls -A "$WORK/elsewhere")"
rm -f "${R:?}/.das-recovery-state"; rmdir "${WORK:?}/elsewhere"

echo "== migration: the plain state, and a crash's .new/.old, set aside; removal in the background"
fixture; mkdir -p "$R/var/lib/das-backup.new" "$R/var/lib/das-backup.old"; echo n >"$R/var/lib/das-backup.new/n"
run
check "migrate: exit 0" test "$RC" -eq 0
check "migrate: state dir left an empty mountpoint" test -d "$R/var/lib/das-backup" -a -z "$(ls -A "$R/var/lib/das-backup")"
check "migrate: plain state aside" test -f "$(find "$R/var/lib/das-backup.migrating" -path '*/state.*/previous' | head -1)"
check "migrate: .new aside" test ! -e "$R/var/lib/das-backup.new" -a -n "$(find "$R/var/lib/das-backup.migrating" -maxdepth 1 -name 'new.*')"
check "migrate: .old aside" test ! -e "$R/var/lib/das-backup.old" -a -n "$(find "$R/var/lib/das-backup.migrating" -maxdepth 1 -name 'old.*')"
check "migrate: removal started after ready, by this script" has "$LOG" "--property=Nice=19 $SCRIPT --remove-set-aside"
check "migrate: nothing removed inline" test -d "$R/var/lib/das-backup.migrating"
fixture; prior_live; mkdir -p "$R/var/lib/das-backup.migrating/state.20261101T000000"; run
check "removal interrupted: started again" has "$LOG" "--remove-set-aside"
fixture; mkdir -p "$R/var/lib/das-backup.migrating/state.x/.snapshots"
run_rm() { RC=0; OUT=$(env DAS_STATE_SYNC_ROOT="$R" bash "$SCRIPT" --remove-set-aside 2>&1) || RC=$?; }
run_rm
check "--remove-set-aside: exit 0" test "$RC" -eq 0
check "--remove-set-aside: removed" test ! -e "$R/var/lib/das-backup.migrating"
check "--remove-set-aside: the state dir untouched" has "$R/var/lib/das-backup/previous" previous
fixture; mkdir -p "$R/var/lib/das-backup.migrating"; rm -f "${R:?}/etc/das-backup/recovery-drive"; run_rm
check "--remove-set-aside without the marker: refused" test "$RC" -eq 1 -a -d "$R/var/lib/das-backup.migrating"

echo "== catch-up that did not start"
fixture
mkdir -p "$R/nvme/root-.20261013T0348/etc/das-backup"
echo "# newer" >"$R/nvme/root-.20261013T0348/etc/das-backup/config.toml"
run STUB_DUMPENV_EMPTY=1
check "no mount: exit 0 (the sync itself succeeded)" test "$RC" -eq 0
check "no mount: no catch-up walk" no_walk
check "no mount: status says NOT started" status_is "synced 20261012T0348; catch-up NOT started: no target mount from dump-env"
fixture
mkdir -p "$R/nvme/root-.20261013T0348/etc/das-backup"
echo "# newer" >"$R/nvme/root-.20261013T0348/etc/das-backup/config.toml"
run STUB_SYSTEMD_RUN_FAIL=1
check "systemd-run fails: exit 0" test "$RC" -eq 0
check "systemd-run fails: status says NOT started" status_is "synced 20261012T0348; catch-up NOT started: systemd-run failed"

echo "== real mount path (ROOT empty, every path through its seam)"
fixture
run DAS_STATE_SYNC_ROOT= DAS_STATE_SYNC_STATE_DIR="$R/var/lib/das-backup" \
    DAS_STATE_SYNC_CONFIG_OUT="$R/etc/das-backup/config.toml" \
    DAS_STATE_SYNC_MARKER="$R/etc/das-backup/recovery-drive" DAS_STATE_SYNC_RUN_DIR="$R/run/das-recovery-state"
check "mount: exit 0" test "$RC" -eq 0
check "mount: top level mounted read-write by subvolid" has "$LOG" "mount -o subvolid=5 /dev/stub $R"
check "mount: unmounted on exit" has "$LOG" "umount $R"
check "mount: state live" has "$LIVE/backup-index.db" db-0348
fixture
run DAS_STATE_SYNC_ROOT= DAS_STATE_SYNC_STATE_DIR="$R/var/lib/das-backup" \
    DAS_STATE_SYNC_CONFIG_OUT="$R/etc/das-backup/config.toml" \
    DAS_STATE_SYNC_MARKER="$R/etc/das-backup/recovery-drive" DAS_STATE_SYNC_RUN_DIR="$R/run/das-recovery-state" \
    STUB_DERIVE_FAIL=1
check "mount (failure): exit 1" test "$RC" -eq 1
check "mount (failure): mounted" has "$LOG" "mount -o subvolid=5 /dev/stub $R"
check "mount (failure): still unmounted on exit" has "$LOG" "umount $R"

echo "== D9: the splash says what is happening, and why"
fixture; rm -f "${WORK:?}/timings"; run
check "d9: exit 0" test "$RC" -eq 0
check "d9: the header is the spec's sentence" has "$LOG" 'display-message --text=DAS recovery disk, Bay 1. Recovery disk boots take longer than a normal boot: this disk is a 2 TB USB drive, and it prepares the latest backup index and settings before the desktop appears. Do not power off or unplug it while it does this.'
check "d9: phases in order" test "$(grep -o 'finding the newest backup\|preparing the configuration\|preparing the backup index\|ready: index from' "$LOG" | tr '\n' '|')" = "finding the newest backup|preparing the configuration|preparing the backup index|ready: index from|"
check "d9: first boot, no countdown (review focus 4)" test "$(grep -c 'measured at the last boot' "$LOG")" -eq 0
check "d9: a measurement was written" grep -q '^total=[0-9]' "$WORK/timings"
check "d9: per-phase measurements" grep -q '^copy=[0-9]' "$WORK/timings"
check "d9: the booted touch file, outside the state dir" test -f "$WORK/booted"
check "d9: the booted file names the provision the marker names" test "$(cat "$WORK/booted")" = "provisioned=20261013-010203"
check "d9: no booted file inside the state dir (it is swapped every boot)" test ! -e "$R/var/lib/das-backup/booted"
check "d9: first boot leaves a database" test -f "$LIVE/backup-index.db"

echo "== D9: the second boot counts down from the measurement"
fixture; printf 'find=1\nconfig=2\ncopy=37\ntotal=40\n' >"$WORK/timings"; run
check "d9 second boot: exit 0" test "$RC" -eq 0
check "d9 second boot: the countdown names its measurement" grep -q 'about [0-9]* s remaining, measured at the last boot' "$LOG"
# the countdown runs on the unit's own clock (bash SECONDS): a slow runner
# spends a second or two before the first phase; the total, less those
check "d9 second boot: the first phase shows the whole measured total" grep -Eq 'finding the newest backup — about (40|39|38) s remaining' "$LOG"
check "d9 second boot: the measurement is replaced, not appended" test "$(grep -c '^total=' "$WORK/timings")" -eq 1

echo "== D9: plymouth absent or not running costs nothing"
fixture; run STUB_PLYMOUTH_DOWN=1
check "d9 no plymouth: exit 0" test "$RC" -eq 0
check "d9 no plymouth: pinged, no messages" test "$(grep -c 'display-message' "$LOG")" -eq 0
check "d9 no plymouth: still synced" status_is "synced 20261012T0348"

echo "== D9: the failure phases"
fixture; rm -rf "${R:?}"/nvme/das-state.*; run
check "d9 no snapshot: exit 2" test "$RC" -eq 2
check "d9 no snapshot: said on the splash" has "$LOG" "no backup received yet"
check "d9 no snapshot: no total written" test ! -e "$WORK/timings"
fixture; rm -rf "${R:?}"/nvme/das-state.* "${R:?}/var/lib/das-backup"; run
check "d9 first boot, no snapshot, no state: exit 2" test "$RC" -eq 2
check "d9 first boot, no snapshot, no state: no database left (review focus 4)" test ! -e "$R/var/lib/das-backup/backup-index.db"
check "d9 first boot, no snapshot, no state: no countdown" test "$(grep -c 'measured at the last boot' "$LOG")" -eq 0
fixture; run STUB_DERIVE_FAIL=1
check "d9 derive fails: said on the splash" has "$LOG" "could not update: derive-recovery refused"
check "d9 derive fails: the booted marker is written all the same" test -f "$WORK/booted"
check "d9 derive fails: no total written (a failed boot measures nothing)" test ! -e "$WORK/timings"
check "d9 derive fails: status still the failure" grep -q '^failed: derive-recovery refused' "$R/run/das-recovery-state/status"

echo "== D9: a marker with no bay line"
fixture; printf 'hand-made\ntarget_subdir=nvme\nroot_series=root-\nstate_series=das-state\n' >"$R/etc/das-backup/recovery-drive"; run
check "d9 hand-made marker: booted is written with an EMPTY provisioned value" test "$(cat "$WORK/booted")" = "provisioned="
check "d9 no bay: exit 0" test "$RC" -eq 0
check "d9 no bay: Bay ?" has "$LOG" "DAS recovery disk, Bay ?."
check "d9 no bay: said in the journal" out_has "the marker has no bay= line"

echo "== D9: a garbled timings file means no countdown, never a stopped sync"
for bad in 08 010 0 abc 99999999999999999999; do
    fixture; printf 'total=%s\n' "$bad" >"$WORK/timings"; run
    check "d9 total=$bad: exit 0" test "$RC" -eq 0
    check "d9 total=$bad: synced" status_is "synced 20261012T0348"
    check "d9 total=$bad: no countdown" test "$(grep -c 'measured at the last boot' "$LOG")" -eq 0
    check "d9 total=$bad: the splash still speaks (every phase shown)" test "$(grep -c 'display-message' "$LOG")" -eq 4
done
fixture; : >"$WORK/timings"; run
check "d9 empty timings: exit 0" test "$RC" -eq 0
check "d9 empty timings: synced" status_is "synced 20261012T0348"
check "d9 empty timings: no countdown" test "$(grep -c 'measured at the last boot' "$LOG")" -eq 0

echo "== D9: the countdown belongs to the working phases only"
fixture; printf 'find=1\nconfig=2\ncopy=37\ntotal=40\n' >"$WORK/timings"; run
check "d9 ready message has no countdown" test "$(grep -c 'ready: index from.*remaining' "$LOG")" -eq 0
check "d9 ready message was shown" has "$LOG" "ready: index from"
fixture; printf 'total=40\n' >"$WORK/timings"; run STUB_DERIVE_FAIL=1
check "d9 failure after a measured boot: shown" has "$LOG" "could not update: derive-recovery refused"
check "d9 failure after a measured boot: no countdown" test "$(grep -c 'could not update.*remaining' "$LOG")" -eq 0
fixture; rm -rf "${R:?}"/nvme/das-state.*; printf 'total=40\n' >"$WORK/timings"; run
check "d9 no snapshot after a measured boot: no countdown" test "$(grep -c 'no backup received yet.*remaining' "$LOG")" -eq 0
# The script counts in $SECONDS, whole wall-clock seconds, so a gap of a few
# milliseconds can read as 1 s when it crosses a second boundary. A budget of
# 1 s therefore left the first phase with 0 s about one run in three on a slow
# runner, and no countdown at all. 3 s leaves the first phase 1-3 s whichever
# way the clock rounds; each splash message takes 4 s, so every later phase
# starts past the budget.
fixture; printf 'total=3\n' >"$WORK/timings"; run STUB_PLYMOUTH_SLOW=4
check "d9 over budget: exit 0" test "$RC" -eq 0
check "d9 over budget: the first phase did count down" grep -qE -- "finding the newest backup — about [1-3] s remaining" "$LOG"
check "d9 over budget: never '0 s remaining'" test "$(grep -c ' 0 s remaining' "$LOG")" -eq 0
check "d9 over budget: no countdown on later phases" test "$(grep -c 'remaining' "$LOG")" -eq 1

echo "== D9: the unit is ordered before both plymouth units that close the splash"
UNIT="$(dirname "$SCRIPT")/../systemd/das-recovery-state-sync.service"
check "unit: After plymouth-start" grep -qE '^After=.*plymouth-start\.service' "$UNIT"
check "unit: Before plymouth-quit.service" grep -qE '^Before=.*plymouth-quit\.service' "$UNIT"
check "unit: Before plymouth-quit-wait.service" grep -qE '^Before=.*plymouth-quit-wait\.service' "$UNIT"

echo "== local documents: the host's copies, from the same root snapshot (bd o77b)"
DOCS=/etc/das-backup-docs
fixture
SNAPDOCS="$R/nvme/root-.20261012T0348$DOCS"
mkdir -p "$SNAPDOCS" "$R$DOCS" "$WORK/outside"
echo "list: Alpha, beta" >"$SNAPDOCS/ref.md"
echo "guide" >"$SNAPDOCS/guide.md"
echo "not a document" >"$SNAPDOCS/notes.txt"
echo "outside the snapshot" >"$WORK/outside/target.md"
ln -s "$WORK/outside/target.md" "$SNAPDOCS/link.md"
echo "list: stale" >"$R$DOCS/ref.md"
echo "kept" >"$R$DOCS/only-on-the-drive.md"
run STUB_DOCS_DIR="$DOCS"
check "docs: sync still exit 0" test "$RC" -eq 0
check "docs: a stale copy replaced" has "$R$DOCS/ref.md" "list: Alpha, beta"
check "docs: a new one copied" has "$R$DOCS/guide.md" "guide"
check "docs: only *.md" test ! -e "$R$DOCS/notes.txt"
check "docs: a symlink is never followed or copied" test ! -e "$R$DOCS/link.md" -a ! -L "$R$DOCS/link.md"
check "docs: nothing is deleted" has "$R$DOCS/only-on-the-drive.md" "kept"
check "docs: no temporary file left" test -z "$(find "$R$DOCS" -name '.*.new')"
check "docs: says how many" out_has "local documents: 2 copied from root-.20261012T0348"

fixture
run STUB_DOCS_DIR="$DOCS"
check "no documents in the snapshot: exit 0" test "$RC" -eq 0
check "no documents in the snapshot: nothing made" test ! -e "$R$DOCS"

fixture
mkdir -p "$R/nvme/root-.20261012T0348/etc/x"
echo "x" >"$R/nvme/root-.20261012T0348/etc/x/a.md"
run STUB_DOCS_DIR="/etc/../etc/x"
check "a docs path with .. : not copied" test ! -e "$R/etc/x/a.md"
check "a docs path with .. : sync still exit 0" test "$RC" -eq 0

# The marker names where the host's snapshots are (bd pzj6): the values come
# from it and nowhere else.
MARKER_F="$R/etc/das-backup/recovery-drive"
echo "== series: other names than nvme, root- and das-state are read from the marker"
fixture
printf 'label=system-recovery-A-2tb\nbay=1\nprovisioned=20261013-010203\ntarget_subdir=hostdir\nroot_series=hostroot-\nstate_series=keepstate\n' >"$MARKER_F"
mv "$R/nvme" "$R/hostdir"
mv "$R/hostdir/root-.20261012T0348" "$R/hostdir/hostroot-.20261012T0348"
mv "$R/hostdir/das-state.20261012T0348" "$R/hostdir/keepstate.20261012T0348"
run
check "names: exit 0" test "$RC" -eq 0
check "names: the state snapshotted from the named series" has "$LOG" "btrfs subvolume snapshot $R/hostdir/keepstate.20261012T0348 $R/.das-recovery-state/next"
check "names: config derived from the named root series" has "$LOG" "--from $R/hostdir/hostroot-.20261012T0348/etc/das-backup/config.toml"
check "names: the state is live" has "$LIVE/backup-index.db" db-0348
check "names: status" status_is "synced 20261012T0348"
fixture
printf 'label=system-recovery-A-2tb\nbay=1\nprovisioned=20261013-010203\ntarget_subdir=hostdir\nroot_series=hostroot-\nstate_series=keepstate\n' >"$MARKER_F"
run
check "names, drive holds the old names only: refused, nothing guessed" test "$RC" -eq 1
check "names, old names on disk: status names the missing directory" status_is "failed: no received 'hostdir' directory on this filesystem: not a target that receives the host's root"
check "names, old names on disk: no snapshot, no swap" file_lacks "$LOG" "btrfs subvolume"

echo "== series: a marker missing one refuses, changing nothing (fail closed)"
for k in target_subdir root_series state_series; do
    fixture; prior_live
    grep -v "^$k=" "$MARKER_F" >"$WORK/marker.new" && mv -f "$WORK/marker.new" "$MARKER_F"
    run
    check "no $k: exit 1" test "$RC" -eq 1
    check "no $k: status names the missing line" grep -q "^failed: the marker .* has no $k= line" "$R/run/das-recovery-state/status"
    check "no $k: said in the journal" out_has "has no $k= line"
    check "no $k: no snapshot, no delete" no_writes
    check "no $k: nothing derived" test ! -e "$R/etc/das-backup/config.toml" -a ! -e "$R/etc/das-backup/config.toml.new"
    check "no $k: the previous live state is mounted again" has "$LOG" "mount -o subvol=/.das-recovery-state/current,nodev,nosuid,noexec"
    check "no $k: and it is the earlier one" has "$LIVE/backup-index.db" db-earlier
    check "no $k: the booted marker is still written" test -f "$WORK/booted"
    # An inherited variable never stands in for a missing line.
    fixture
    grep -v "^$k=" "$MARKER_F" >"$WORK/marker.new" && mv -f "$WORK/marker.new" "$MARKER_F"
    run DAS_STATE_SYNC_SUBDIR=nvme TARGET_SUBDIR=nvme ROOT_SERIES=root- STATE_SERIES=das-state k="$k"
    check "no $k, environment set: still refused" test "$RC" -eq 1
done
fixture; printf 'label=system-recovery-A-2tb\nbay=1\nprovisioned=20261013-010203\ntarget_subdir=\nroot_series=root-\nstate_series=das-state\n' >"$MARKER_F"
run
check "empty target_subdir: refused, not read as the volume's top level" test "$RC" -eq 1
check "empty target_subdir: no snapshot" file_lacks "$LOG" "btrfs subvolume"

echo "== series: a value that is not a plain name refuses"
# shellcheck disable=SC2016 # '$x' is a literal value to refuse
for bad in '..' '../x' '/abs' 'a/../b' 'a//b' '.' 'a b' 'a*' 'a;b' '$x'; do
    for k in target_subdir root_series state_series; do
        fixture
        printf 'bay=1\ntarget_subdir=nvme\nroot_series=root-\nstate_series=das-state\n' >"$MARKER_F"
        sed -i "s|^$k=.*|$k=$bad|" "$MARKER_F"
        run
        check "$k='$bad': refused (exit 1)" test "$RC" -eq 1
        check "$k='$bad': no snapshot, nothing mounted" file_lacks "$LOG" "btrfs subvolume"
    done
done
for k in root_series state_series; do
    fixture
    printf 'bay=1\ntarget_subdir=nvme\nroot_series=root-\nstate_series=das-state\n' >"$MARKER_F"
    sed -i "s|^$k=.*|$k=a/b|" "$MARKER_F"
    run
    check "$k holding a slash: refused" test "$RC" -eq 1
done
fixture
printf 'bay=1\ntarget_subdir=sub/dir\nroot_series=root-\nstate_series=das-state\n' >"$MARKER_F"
mkdir -p "$R/sub" && mv "$R/nvme" "$R/sub/dir"
run
check "target_subdir may be a relative path of plain names" test "$RC" -eq 0

echo "== series: a dotted sibling series is not this series (bd eg4j.15 note 1)"
fixture
mkdir -p "$R/nvme/das-state.extra.20261014T0348" "$R/nvme/root-.extra.20261014T0348/etc/das-backup" "$R/nvme/das-state.20261012T0348.bak"
echo db-extra >"$R/nvme/das-state.extra.20261014T0348/backup-index.db"
echo "# extra" >"$R/nvme/root-.extra.20261014T0348/etc/das-backup/config.toml"
run
check "dotted sibling: exit 0" test "$RC" -eq 0
check "dotted sibling: the state is the series' own newest" has "$LIVE/backup-index.db" db-0348
check "dotted sibling: the sibling state is not snapshotted" file_lacks "$LOG" "das-state.extra"
check "dotted sibling: the config is the series' own newest" has "$LOG" "--from $R/nvme/root-.20261012T0348/etc/das-backup/config.toml"
check "dotted sibling: a name after the stamp is not a stamp" file_lacks "$LOG" "das-state.20261012T0348.bak"
fixture
mkdir -p "$R/nvme/das-state.20261012T0348_1" "$R/nvme/das-state.20261012T034815"
echo db-n1 >"$R/nvme/das-state.20261012T0348_1/backup-index.db"
run
check "stamp forms: a _N suffix and a seconds stamp are stamps" has "$LOG" "btrfs subvolume snapshot $R/nvme/das-state.20261012T0348_1 "

echo "== series: a symlink in the middle of target_subdir refuses (bd eg4j.15 note 2)"
fixture
printf 'bay=1\ntarget_subdir=sub/dir\nroot_series=root-\nstate_series=das-state\n' >"$MARKER_F"
mkdir -p "$R/real" && mv "$R/nvme" "$R/real/dir" && ln -s real "$R/sub"
run
check "middle symlink: refused (exit 1)" test "$RC" -eq 1
check "middle symlink: no snapshot" file_lacks "$LOG" "btrfs subvolume"
check "middle symlink: says which directory" out_has "no received 'sub/dir' directory"

if ((FAILS > 0)); then
    printf '%d check(s) FAILED\n' "$FAILS"
    exit 1
fi
echo "RECOVERY-STATE-SYNC SUITE GREEN"
