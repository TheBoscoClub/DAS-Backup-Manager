#!/bin/bash
#
# tests/test_recovery_state_sync.sh
#
# scripts/das-recovery-state-sync.sh against a fixture tree and stub findmnt,
# btrdasd, systemd-run, mount, umount, btrfs and mv. Runs as an ordinary user;
# nothing is mounted. Every case starts from a fixture reset, and every tool
# the script calls is reached through its DAS_STATE_SYNC_* seam.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/das-recovery-state-sync.sh"
WORK=$(mktemp -d)
trap 'chmod -R u+rwX "$WORK" 2>/dev/null || :; rm -rf "$WORK"' EXIT
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
no_leftovers() { test ! -e "$R/var/lib/das-backup.new" -a ! -e "$R/var/lib/das-backup.old" -a ! -e "$R/etc/das-backup/config.toml.new"; }

mkdir -p "$BIN"
# findmnt -no UUID,FSROOT -M <mp>: the own target's mount state. STUB_MNT (a
# full "UUID FSROOT" line) forces it already mounted as that; otherwise it is
# mounted (as this drive's top level) only after the mount stub ran for it.
cat >"$BIN/findmnt" <<S
#!/bin/bash
case "\$*" in
*FSROOT*)
    if [[ -n "\${STUB_MNT:-}" ]]; then echo "\$STUB_MNT"
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
[[ -z "${STUB_MOUNT_FAIL:-}" ]] || exit 32
[[ "$*" != *UUID=* ]] || : >"$STUB_LOG.mounted"
S
cat >"$BIN/umount" <<'S'
#!/bin/bash
echo "umount $*" >>"$STUB_LOG"
S
# btrfs subvolume show <path>: complete (read-only + Received UUID) unless the
# path contains $STUB_PARTIAL, which then reads as an interrupted receive.
cat >"$BIN/btrfs" <<'S'
#!/bin/bash
echo "btrfs $*" >>"$STUB_LOG"
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
[[ -z "${STUB_PLYMOUTH_SLOW:-}" || "${1:-}" != display-message ]] || sleep 2
[[ "${1:-}" != --ping || -z "${STUB_PLYMOUTH_DOWN:-}" ]]
S
chmod +x "$BIN"/*

fixture() {
    chmod -R u+rwX "$R" 2>/dev/null || :
    rm -rf "$R"
    : >"$LOG"
    rm -f "$WORK/mv-count" "$LOG.mounted" "$WORK/timings" "$WORK/booted"
    mkdir -p "$R/etc/das-backup" "$R/run" "$R/var/lib/das-backup" \
        "$R/nvme/root-.20261012T0348/etc/das-backup" "$R/nvme/das-state.20261012T0348"
    printf 'label=system-recovery-A-2tb\nbay=1\nprovisioned=20261013-010203\n' >"$R/etc/das-backup/recovery-drive"
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

echo "== synced"
fixture
run
check "synced: exit 0" test "$RC" -eq 0
check "synced: index copied" has "$R/var/lib/das-backup/backup-index.db" db-0348
check "synced: previous state replaced" test ! -e "$R/var/lib/das-backup/previous"
check "synced: config derived" has "$R/etc/das-backup/config.toml" "recovery_drive = true"
check "synced: status" status_is "synced 20261012T0348"
check "synced: derive-recovery args" has "$LOG" "--booted-uuid $UUID_STUB"
check "synced: derive-recovery source" has "$LOG" "--from $R/nvme/root-.20261012T0348/etc/das-backup/config.toml"
check "synced: no catch-up" file_lacks "$LOG" systemd-run
check "synced: no mount/umount under the ROOT seam" file_lacks "$LOG" "mount "
check "synced: no leftovers" no_leftovers

echo "== gap (state older than data)"
fixture
mkdir -p "$R/nvme/root-.20261013T0348/etc/das-backup"
echo "# newer" >"$R/nvme/root-.20261013T0348/etc/das-backup/config.toml"
run
check "gap: exit 0" test "$RC" -eq 0
check "gap: older state copied" has "$R/var/lib/das-backup/backup-index.db" db-0348
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
check "mount fails: no catch-up" file_lacks "$LOG" systemd-run
check "mount fails: status says NOT started" status_is "synced 20261012T0348; catch-up NOT started: cannot mount $WALK_MOUNT"

echo "== state newer than data (the normal nightly layout)"
fixture
mkdir -p "$R/nvme/das-state.20261012T0412"
echo db-0412 >"$R/nvme/das-state.20261012T0412/backup-index.db"
run
check "newer: exit 0" test "$RC" -eq 0
check "newer: newest state copied" has "$R/var/lib/das-backup/backup-index.db" db-0412
check "newer: no catch-up" file_lacks "$LOG" systemd-run
check "newer: status" status_is "synced 20261012T0412"

echo "== partial receive skipped"
fixture
mkdir -p "$R/nvme/das-state.20261012T0412"
echo db-partial >"$R/nvme/das-state.20261012T0412/backup-index.db"
run STUB_PARTIAL="das-state.20261012T0412"
check "partial: exit 0" test "$RC" -eq 0
check "partial: previous complete state used" has "$R/var/lib/das-backup/backup-index.db" db-0348
check "partial: skip is logged" out_has "skipping das-state.20261012T0412"
check "partial: status names the complete one" status_is "synced 20261012T0348"

echo "== no state snapshot"
fixture
rm -rf "$R"/nvme/das-state.*
run
check "nosnap: exit 2 (the unit's SuccessExitStatus), not 1" test "$RC" -eq 2
check "nosnap: the unit lists exit 2 as success" grep -q '^SuccessExitStatus=2$' "$(dirname "$SCRIPT")/../systemd/das-recovery-state-sync.service"
check "nosnap: previous state intact" has "$R/var/lib/das-backup/previous" previous
check "nosnap: no config written" test ! -e "$R/etc/das-backup/config.toml"
check "nosnap: status" status_is "no state snapshot"
check "nosnap: the booted marker is written on a boot that synced nothing" test "$(cat "$WORK/booted")" = "provisioned=20261013-010203"

echo "== no marker"
fixture
rm -f "$R/etc/das-backup/recovery-drive" "$WORK/booted"
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

echo "== copy fails"
if [[ $(id -u) -eq 0 ]]; then
    echo "skip: running as root, chmod 000 does not stop the copy"
else
    fixture
    chmod 000 "$R/nvme/das-state.20261012T0348/backup-index.db"
    run
    check "copy: exit 1" test "$RC" -eq 1
    check "copy: previous state intact" has "$R/var/lib/das-backup/previous" previous
    check "copy: no config written" test ! -e "$R/etc/das-backup/config.toml"
    check "copy: no leftovers" no_leftovers
    check "copy: status" status_is "failed: copying das-state.20261012T0348 failed: state left as it was"
fi

echo "== swap (mv) fails"
fixture
run STUB_MV_FAIL_AT="1"
check "mv: exit 1" test "$RC" -eq 1
check "mv: previous state intact" has "$R/var/lib/das-backup/previous" previous
check "mv: index not installed" test ! -e "$R/var/lib/das-backup/backup-index.db"
check "mv: no config written" test ! -e "$R/etc/das-backup/config.toml"
check "mv: no leftovers" no_leftovers
check "mv: status is a failure" grep -q '^failed: ' "$R/run/das-recovery-state/status"

echo "== swap fallback (no mv --exchange)"
fixture
run STUB_MV_NO_EXCHANGE=1
check "fallback: exit 0" test "$RC" -eq 0
check "fallback: state replaced" has "$R/var/lib/das-backup/backup-index.db" db-0348
check "fallback: previous gone" test ! -e "$R/var/lib/das-backup/previous"
check "fallback: no leftovers" no_leftovers
fixture
run STUB_MV_NO_EXCHANGE=1 STUB_MV_FAIL_AT="2"
check "fallback 2nd move fails: exit 1" test "$RC" -eq 1
check "fallback 2nd move fails: previous state restored" has "$R/var/lib/das-backup/previous" previous
check "fallback 2nd move fails: new state not installed" test ! -e "$R/var/lib/das-backup/backup-index.db"
check "fallback 2nd move fails: no leftovers" no_leftovers
check "fallback 2nd move fails: status" status_is "failed: installing the new state failed: previous state restored"
fixture
run STUB_MV_NO_EXCHANGE=1 STUB_MV_FAIL_AT="2 3"
check "fallback undo fails: exit 1" test "$RC" -eq 1
check "fallback undo fails: previous state kept in .old" has "$R/var/lib/das-backup.old/previous" previous
check "fallback undo fails: never claims restored" file_lacks "$R/run/das-recovery-state/status" "previous state restored"
check "fallback undo fails: status says where it is" \
    status_is "failed: swap failed and the previous state could not be restored: it is in $R/var/lib/das-backup.old"
check "fallback undo fails: no .new left" test ! -e "$R/var/lib/das-backup.new"

echo "== catch-up that did not start"
fixture
mkdir -p "$R/nvme/root-.20261013T0348/etc/das-backup"
echo "# newer" >"$R/nvme/root-.20261013T0348/etc/das-backup/config.toml"
run STUB_DUMPENV_EMPTY=1
check "no mount: exit 0 (the sync itself succeeded)" test "$RC" -eq 0
check "no mount: systemd-run not called" file_lacks "$LOG" systemd-run
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
check "mount: top level mounted read-only by subvolid" has "$LOG" "mount -o subvolid=5,ro /dev/stub $R"
check "mount: unmounted on exit" has "$LOG" "umount $R"
check "mount: state copied" has "$R/var/lib/das-backup/backup-index.db" db-0348
fixture
run DAS_STATE_SYNC_ROOT= DAS_STATE_SYNC_STATE_DIR="$R/var/lib/das-backup" \
    DAS_STATE_SYNC_CONFIG_OUT="$R/etc/das-backup/config.toml" \
    DAS_STATE_SYNC_MARKER="$R/etc/das-backup/recovery-drive" DAS_STATE_SYNC_RUN_DIR="$R/run/das-recovery-state" \
    STUB_DERIVE_FAIL=1
check "mount (failure): exit 1" test "$RC" -eq 1
check "mount (failure): mounted" has "$LOG" "mount -o subvolid=5,ro /dev/stub $R"
check "mount (failure): still unmounted on exit" has "$LOG" "umount $R"

echo "== D9: the splash says what is happening, and why"
fixture; rm -f "$WORK/timings"; run
check "d9: exit 0" test "$RC" -eq 0
check "d9: the header is the spec's sentence" has "$LOG" 'display-message --text=DAS recovery disk, Bay 1. Recovery disk boots take significantly longer than a normal boot: this disk is a 2 TB USB drive, and it copies the latest backup index and settings before the desktop appears. Do not power off or unplug it — the copy would be left incomplete and the disk would start again from the previous copy.'
check "d9: phases in order" test "$(grep -o 'finding the newest backup\|preparing the configuration\|copying the backup index\|ready: index from' "$LOG" | tr '\n' '|')" = "finding the newest backup|preparing the configuration|copying the backup index|ready: index from|"
check "d9: first boot, no countdown (review focus 4)" test "$(grep -c 'measured at the last boot' "$LOG")" -eq 0
check "d9: a measurement was written" grep -q '^total=[0-9]' "$WORK/timings"
check "d9: per-phase measurements" grep -q '^copy=[0-9]' "$WORK/timings"
check "d9: the booted touch file, outside the state dir" test -f "$WORK/booted"
check "d9: the booted file names the provision the marker names" test "$(cat "$WORK/booted")" = "provisioned=20261013-010203"
check "d9: no booted file inside the state dir (it is swapped every boot)" test ! -e "$R/var/lib/das-backup/booted"
check "d9: first boot leaves a database" test -f "$R/var/lib/das-backup/backup-index.db"

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
fixture; rm -rf "$R"/nvme/das-state.*; run
check "d9 no snapshot: exit 2" test "$RC" -eq 2
check "d9 no snapshot: said on the splash" has "$LOG" "no backup received yet"
check "d9 no snapshot: no total written" test ! -e "$WORK/timings"
fixture; rm -rf "$R"/nvme/das-state.* "$R/var/lib/das-backup"; run
check "d9 first boot, no snapshot, no state: exit 2" test "$RC" -eq 2
check "d9 first boot, no snapshot, no state: no database left (review focus 4)" test ! -e "$R/var/lib/das-backup/backup-index.db"
check "d9 first boot, no snapshot, no state: no countdown" test "$(grep -c 'measured at the last boot' "$LOG")" -eq 0
fixture; run STUB_DERIVE_FAIL=1
check "d9 derive fails: said on the splash" has "$LOG" "could not update: derive-recovery refused"
check "d9 derive fails: the booted marker is written all the same" test -f "$WORK/booted"
check "d9 derive fails: no total written (a failed boot measures nothing)" test ! -e "$WORK/timings"
check "d9 derive fails: status still the failure" grep -q '^failed: derive-recovery refused' "$R/run/das-recovery-state/status"

echo "== D9: a marker with no bay line"
fixture; printf 'hand-made\n' >"$R/etc/das-backup/recovery-drive"; run
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
fixture; rm -rf "$R"/nvme/das-state.*; printf 'total=40\n' >"$WORK/timings"; run
check "d9 no snapshot after a measured boot: no countdown" test "$(grep -c 'no backup received yet.*remaining' "$LOG")" -eq 0
fixture; printf 'total=1\n' >"$WORK/timings"; run STUB_PLYMOUTH_SLOW=1
check "d9 over budget: exit 0" test "$RC" -eq 0
check "d9 over budget: the first phase did count down" has "$LOG" "finding the newest backup — about 1 s remaining"
check "d9 over budget: never '0 s remaining'" test "$(grep -c ' 0 s remaining' "$LOG")" -eq 0
check "d9 over budget: no countdown on later phases" test "$(grep -c 'remaining' "$LOG")" -eq 1

echo "== D9: the unit is ordered before both plymouth units that close the splash"
UNIT="$(dirname "$SCRIPT")/../systemd/das-recovery-state-sync.service"
check "unit: After plymouth-start" grep -qE '^After=.*plymouth-start\.service' "$UNIT"
check "unit: Before plymouth-quit.service" grep -qE '^Before=.*plymouth-quit\.service' "$UNIT"
check "unit: Before plymouth-quit-wait.service" grep -qE '^Before=.*plymouth-quit-wait\.service' "$UNIT"

if ((FAILS > 0)); then
    printf '%d check(s) FAILED\n' "$FAILS"
    exit 1
fi
echo "RECOVERY-STATE-SYNC SUITE GREEN"
