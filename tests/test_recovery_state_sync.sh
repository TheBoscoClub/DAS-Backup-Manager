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
hasnt() { ! grep -qF -- "$2" "$1" 2>/dev/null; }
out_has() { grep -qF -- "$1" <<<"$OUT"; }
status_is() { [[ "$(cat "$R/run/das-recovery-state/status" 2>/dev/null)" == "$1" ]]; }
no_leftovers() { test ! -e "$R/var/lib/das-backup.new" -a ! -e "$R/var/lib/das-backup.old" -a ! -e "$R/etc/das-backup/config.toml.new"; }

mkdir -p "$BIN"
cat >"$BIN/findmnt" <<S
#!/bin/bash
case "\$*" in
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
elif [[ "\$1 \$2" == "config dump-env" ]]; then
    echo "DAS_TARGET_COUNT=2"
    echo "DAS_TARGET_0_MOUNT='$WALK_MOUNT'"
fi
S
cat >"$BIN/systemd-run" <<'S'
#!/bin/bash
echo "systemd-run $*" >>"$STUB_LOG"
S
cat >"$BIN/mount" <<'S'
#!/bin/bash
echo "mount $*" >>"$STUB_LOG"
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
# mv: passes through to the real one; with STUB_MV_FAIL=<flagfile> the first
# real call (not --help) fails once.
cat >"$BIN/mv" <<'S'
#!/bin/bash
if [[ "${1:-}" != --help && -n "${STUB_MV_FAIL:-}" && ! -e "$STUB_MV_FAIL" ]]; then
    : >"$STUB_MV_FAIL"
    echo "mv: stub failure" >&2
    exit 1
fi
exec /usr/bin/mv "$@"
S
chmod +x "$BIN"/*

fixture() {
    chmod -R u+rwX "$R" 2>/dev/null || :
    rm -rf "$R"
    : >"$LOG"
    mkdir -p "$R/etc/das-backup" "$R/run" "$R/var/lib/das-backup" \
        "$R/nvme/root-.20261012T0348/etc/das-backup" "$R/nvme/das-state.20261012T0348"
    : >"$R/etc/das-backup/recovery-drive"
    echo previous >"$R/var/lib/das-backup/previous"
    echo "# host config" >"$R/nvme/root-.20261012T0348/etc/das-backup/config.toml"
    echo db-0348 >"$R/nvme/das-state.20261012T0348/backup-index.db"
}

RC=0
OUT=
run() { # run [VAR=value ...]: the script under the seams; RC and OUT hold the result
    RC=0
    OUT=$(env PATH="$BIN:$PATH" STUB_LOG="$LOG" DAS_STATE_SYNC_ROOT="$R" DAS_STATE_SYNC_TOP="$R" \
        DAS_STATE_SYNC_BTRDASD="$BIN/btrdasd" DAS_STATE_SYNC_FINDMNT="$BIN/findmnt" \
        DAS_STATE_SYNC_SYSTEMD_RUN="$BIN/systemd-run" DAS_STATE_SYNC_MOUNT="$BIN/mount" \
        DAS_STATE_SYNC_UMOUNT="$BIN/umount" DAS_STATE_SYNC_BTRFS="$BIN/btrfs" \
        DAS_STATE_SYNC_MV="$BIN/mv" "$@" bash "$SCRIPT" 2>&1) || RC=$?
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
check "synced: no catch-up" hasnt "$LOG" systemd-run
check "synced: no mount/umount under the ROOT seam" hasnt "$LOG" "mount "
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
check "gap: walk is NOT given the top-level mount" hasnt "$LOG" "walk $R"

echo "== state newer than data (the normal nightly layout)"
fixture
mkdir -p "$R/nvme/das-state.20261012T0412"
echo db-0412 >"$R/nvme/das-state.20261012T0412/backup-index.db"
run
check "newer: exit 0" test "$RC" -eq 0
check "newer: newest state copied" has "$R/var/lib/das-backup/backup-index.db" db-0412
check "newer: no catch-up" hasnt "$LOG" systemd-run
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
check "nosnap: exit 1" test "$RC" -eq 1
check "nosnap: previous state intact" has "$R/var/lib/das-backup/previous" previous
check "nosnap: no config written" test ! -e "$R/etc/das-backup/config.toml"
check "nosnap: status" status_is "no state snapshot"

echo "== no marker"
fixture
rm -f "$R/etc/das-backup/recovery-drive"
run
check "nomarker: exit 1" test "$RC" -eq 1
check "nomarker: message" out_has "not a recovery drive"
check "nomarker: previous state intact" has "$R/var/lib/das-backup/previous" previous
check "nomarker: no config written" test ! -e "$R/etc/das-backup/config.toml"
check "nomarker: no status written" test ! -e "$R/run/das-recovery-state/status"
check "nomarker: no tools called" test ! -s "$LOG"

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
run STUB_MV_FAIL="$WORK/mv-failed"
check "mv: exit 1" test "$RC" -eq 1
check "mv: previous state intact" has "$R/var/lib/das-backup/previous" previous
check "mv: index not installed" test ! -e "$R/var/lib/das-backup/backup-index.db"
check "mv: no config written" test ! -e "$R/etc/das-backup/config.toml"
check "mv: no leftovers" no_leftovers
check "mv: status is a failure" grep -q '^failed: ' "$R/run/das-recovery-state/status"

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

if ((FAILS > 0)); then
    printf '%d check(s) FAILED\n' "$FAILS"
    exit 1
fi
echo "RECOVERY-STATE-SYNC SUITE GREEN"
