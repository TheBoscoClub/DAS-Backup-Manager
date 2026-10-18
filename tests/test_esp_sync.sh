#!/bin/bash
# shellcheck disable=SC2016
# SC2016: the single-quoted text below is the source of stub programs.
#
# tests/test_esp_sync.sh
#
# esp-sync.sh, the one mechanism that mirrors the primary ESP (/boot,
# LABEL=EFI) onto the backup NVMe ESP (/mnt/esp-backup, LABEL=EFI-BACKUP),
# run end to end in a sandbox (bd DAS-Backup-Manager-he59). Its tracked source
# is live/usr/local/lib/das-backup-manager/esp-sync.sh.
#
# The script names /boot and /mnt/esp-backup as constants and has no override,
# on purpose: nothing a caller sets can point it at another device. So the
# suite never runs the script in place. It copies it beneath a mktemp
# directory and rewrites exactly those two constant lines to scratch
# directories (refusing if either line is not found exactly once, or if any
# non-comment line still names a real ESP path); every other line runs as
# written. blkid, findmnt, mountpoint, mount and logger are stubs that print
# canned answers and never call a real binary, so the device each ESP "is on"
# is whatever a case says. No root, no devices, no real ESP is read or written.
#
# Cases:
#   - the NVMe-only refusal fires for a /dev/sd* device, as the backup ESP
#     (the 2026-03-05 shape) and as the primary, and nothing is written
#   - the label <-> mount cross-check refuses a mount on another device
#   - a dry run reports what it would do and changes nothing
#   - a real run copies new and changed files, removes a stale file and the
#     directory it leaves empty, and keeps the per-ESP random-seed
#   - the two guarded deletions: an empty ESP_BACKUP, and an empty path
#     reaching the stale-file rm, abort the script instead of passing quietly
#
# ESP_SYNC_UNDER_TEST names another copy of the script to run instead (used to
# watch each case fail against a script with its guard taken out).
set -uo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="${ESP_SYNC_UNDER_TEST:-$HERE/../live/usr/local/lib/das-backup-manager/esp-sync.sh}"
[[ -f "$SCRIPT" ]] || {
    echo "FAIL setup -- script under test not found: $SCRIPT"
    exit 1
}

WORK="$(mktemp -d)" && [[ -d "$WORK" ]] || {
    echo "FAIL setup -- mktemp -d failed"
    exit 1
}
trap 'rm -rf "${WORK:?}"' EXIT

fails=0
pass() { echo "PASS $1"; }
fail() {
    echo "FAIL $1 -- $2"
    fails=$((fails + 1))
}

# --- stubs: canned answers only, none calls a real binary of its own name ---
STUBS="$WORK/stubs"
mkdir -p "$STUBS"
# blkid -t LABEL=<label> -o device
printf '%s\n' '#!/bin/bash' '
[[ $# -eq 4 && $1 == -t && $3 == -o && $4 == device ]] || { echo "blkid stub: unexpected: $*" >&2; exit 64; }
case $2 in
    LABEL=EFI) echo "$STUB_LABEL_EFI" ;;
    LABEL=EFI-BACKUP) echo "$STUB_LABEL_EFI_BACKUP" ;;
    *) exit 2 ;;
esac' >"$STUBS/blkid"
# findmnt -n -o SOURCE|FSTYPE|OPTIONS <mountpoint>
printf '%s\n' '#!/bin/bash' '
[[ $# -eq 4 && $1 == -n && $2 == -o ]] || { echo "findmnt stub: unexpected: $*" >&2; exit 64; }
if [[ $4 == "$STUB_PRIMARY_MP" ]]; then dev=$STUB_PRIMARY_DEV; else dev=$STUB_BACKUP_DEV; fi
case $3 in
    SOURCE) echo "$dev" ;;
    FSTYPE) echo vfat ;;
    OPTIONS) echo rw,relatime ;;
    *) echo "findmnt stub: unexpected column $3" >&2; exit 64 ;;
esac' >"$STUBS/findmnt"
printf '%s\n' '#!/bin/bash' 'exit 0' >"$STUBS/mountpoint"
printf '%s\n' '#!/bin/bash' 'echo "mount stub: refused: $*" >&2' 'exit 1' >"$STUBS/mount"
printf '%s\n' '#!/bin/bash' 'printf "%s\n" "$*" >>"$STUB_LOG"' >"$STUBS/logger"
# Only on PATH for the empty-path case: the real find, plus one empty record
# after its output when it lists, NUL-separated, the directory
# STUB_FIND_INJECT_FOR names (the stale-file scan; not the -delete pass).
mkdir -p "$WORK/findstub"
printf '%s\n' '#!/bin/bash' '
/usr/bin/find "$@"
rc=$?
[[ $1 == "$STUB_FIND_INJECT_FOR" && ${*: -1} == -print0 ]] && printf "\0"
exit "$rc"' >"$WORK/findstub/find"
chmod +x "$STUBS"/* "$WORK/findstub/find"

# A copy of the script whose two ESP constants name $2 and $3.
prepare() {
    local out=$1 primary=$2 backup=$3
    local n_p n_b
    n_p=$(grep -c '^ESP_PRIMARY="/boot"$' "$SCRIPT")
    n_b=$(grep -c '^ESP_BACKUP="/mnt/esp-backup"$' "$SCRIPT")
    if [[ "$n_p" != 1 || "$n_b" != 1 ]]; then
        echo "FAIL setup -- ESP constant lines found $n_p and $n_b times, want 1 and 1"
        exit 1
    fi
    sed -e "s|^ESP_PRIMARY=\"/boot\"\$|ESP_PRIMARY=\"$primary\"|" \
        -e "s|^ESP_BACKUP=\"/mnt/esp-backup\"\$|ESP_BACKUP=\"$backup\"|" "$SCRIPT" >"$out"
    if grep -vE '^[[:space:]]*#' "$out" | grep -qE '/boot|/mnt/esp-backup'; then
        echo "FAIL setup -- the sandbox copy still names a real ESP path outside comments"
        exit 1
    fi
}

# Every path, type and checksum under a directory, for before/after compares.
tree_state() {
    (cd "$1" && find . -printf '%y %p\n' | LC_ALL=C sort && find . -type f -exec md5sum {} + | LC_ALL=C sort)
}

# run <case dir> <primary dev> <backup dev> [script args...]: sets OUT ERR RC.
# Labels resolve to the mounted devices unless STUB_LABEL_* are set by the case.
run() {
    local c=$1 pdev=$2 bdev=$3
    shift 3
    OUT=$(env PATH="${EXTRA_PATH:+$EXTRA_PATH:}$STUBS:$PATH" \
        STUB_PRIMARY_MP="$c/primary" STUB_PRIMARY_DEV="$pdev" STUB_BACKUP_DEV="$bdev" \
        STUB_LABEL_EFI="${LABEL_EFI:-$pdev}" STUB_LABEL_EFI_BACKUP="${LABEL_EFI_BACKUP:-$bdev}" \
        STUB_LOG="$c/logger.log" STUB_FIND_INJECT_FOR="$c/backup" \
        bash "$c/esp-sync.sh" "$@" 2>"$c/stderr")
    RC=$?
    ERR=$(<"$c/stderr")
}

# Two ESP trees: one file the same, one new, one changed, a stale file alone
# in its directory, and a random-seed that differs (per-ESP, never synced).
populate() {
    local c=$1
    mkdir -p "$c/primary/EFI/BOOT" "$c/primary/loader/entries" \
        "$c/backup/EFI/BOOT" "$c/backup/loader" "$c/backup/stale"
    echo same >"$c/primary/EFI/BOOT/BOOTX64.EFI"
    echo same >"$c/backup/EFI/BOOT/BOOTX64.EFI"
    echo new >"$c/primary/loader/entries/new.conf"
    echo kernel-new >"$c/primary/vmlinuz"
    echo kernel-old >"$c/backup/vmlinuz"
    echo stale >"$c/backup/stale/old.conf"
    echo seed-primary >"$c/primary/loader/random-seed"
    echo seed-backup >"$c/backup/loader/random-seed"
}

NVME_P=/dev/nvme9n1p9
NVME_B=/dev/nvme8n1p9
SATA=/dev/sdz9

echo "--- the NVMe-only refusal"
for role in backup primary; do
    c="$WORK/nvme-$role"
    mkdir -p "$c"
    populate "$c"
    prepare "$c/esp-sync.sh" "$c/primary" "$c/backup"
    before=$(tree_state "$c/backup")
    if [[ $role == backup ]]; then run "$c" "$NVME_P" "$SATA"; else run "$c" "$SATA" "$NVME_B"; fi
    name="nvme-only refusal, $role ESP on $SATA"
    if [[ $RC -eq 0 ]]; then
        fail "$name" "exit 0, want a refusal; stdout: $OUT"
    elif [[ "$ERR" != *"BLOCKED: $SATA is not an NVMe device"* ]]; then
        fail "$name" "exit $RC without the BLOCKED message; stderr: $ERR"
    elif [[ "$(tree_state "$c/backup")" != "$before" ]]; then
        fail "$name" "the backup tree changed"
    elif [[ "$OUT" == *"ESP sync complete"* ]]; then
        fail "$name" "it reported a completed sync"
    else
        pass "$name"
    fi
done

echo "--- the label <-> mount cross-check"
c="$WORK/mismatch"
mkdir -p "$c"
populate "$c"
prepare "$c/esp-sync.sh" "$c/primary" "$c/backup"
before=$(tree_state "$c/backup")
LABEL_EFI_BACKUP=/dev/nvme7n1p9 run "$c" "$NVME_P" "$NVME_B"
name="backup mounted from a device other than LABEL=EFI-BACKUP"
if [[ $RC -eq 0 ]]; then
    fail "$name" "exit 0, want a refusal; stdout: $OUT"
elif [[ "$ERR" != *"Mount/label mismatch"* ]]; then
    fail "$name" "exit $RC without the mismatch message; stderr: $ERR"
elif [[ "$(tree_state "$c/backup")" != "$before" ]]; then
    fail "$name" "the backup tree changed"
else
    pass "$name"
fi

echo "--- a dry run"
c="$WORK/dry"
mkdir -p "$c"
populate "$c"
prepare "$c/esp-sync.sh" "$c/primary" "$c/backup"
before=$(tree_state "$c/backup")
run "$c" "$NVME_P" "$NVME_B" --dry-run
name="dry run reports and changes nothing"
want_lines=$'DRY-RUN: would copy loader/entries/new.conf\nDRY-RUN: would copy vmlinuz\nDRY-RUN: would remove stale stale/old.conf'
got_lines=$(grep '^DRY-RUN:' <<<"$OUT")
if [[ $RC -ne 0 ]]; then
    fail "$name" "exit $RC; stderr: $ERR"
elif [[ "$got_lines" != "$want_lines" ]]; then
    fail "$name" "DRY-RUN lines: [$got_lines], want [$want_lines]"
elif [[ "$(tree_state "$c/backup")" != "$before" ]]; then
    fail "$name" "the backup tree changed in a dry run"
elif [[ "$OUT" != *"ESP sync complete: 0 copied, 2 unchanged, 0 removed, 0 errors"* ]]; then
    fail "$name" "summary: $OUT"
else
    pass "$name"
fi

echo "--- a real run"
c="$WORK/real"
mkdir -p "$c"
populate "$c"
prepare "$c/esp-sync.sh" "$c/primary" "$c/backup"
run "$c" "$NVME_P" "$NVME_B"
name="real run copies, removes stale, keeps random-seed"
if [[ $RC -ne 0 ]]; then
    fail "$name" "exit $RC; stderr: $ERR"
elif ! cmp -s "$c/primary/loader/entries/new.conf" "$c/backup/loader/entries/new.conf"; then
    fail "$name" "the new file was not copied"
elif ! cmp -s "$c/primary/vmlinuz" "$c/backup/vmlinuz"; then
    fail "$name" "the changed file was not copied"
elif [[ -e "$c/backup/stale" ]]; then
    fail "$name" "the stale file or its emptied directory is still there"
elif [[ "$(<"$c/backup/loader/random-seed")" != seed-backup ]]; then
    fail "$name" "the backup's own random-seed was overwritten"
elif [[ "$OUT" != *"ESP sync complete: 2 copied, 2 unchanged, 1 removed, 0 errors"* ]]; then
    fail "$name" "summary: $OUT"
else
    pass "$name"
fi

echo "--- the guarded deletions"
c="$WORK/empty-backup"
mkdir -p "$c/primary"
prepare "$c/esp-sync.sh" "$c/primary" ""
run "$c" "$NVME_P" "$NVME_B"
name="empty ESP_BACKUP aborts before the empty-directory find -delete"
if [[ $RC -eq 0 ]]; then
    fail "$name" "exit 0, want an abort; stdout: $OUT"
elif [[ "$ERR" != *"ESP_BACKUP: parameter null or not set"* ]]; then
    fail "$name" "exit $RC without the parameter-null abort; stderr: $ERR"
elif [[ "$OUT" == *"ESP sync complete"* ]]; then
    fail "$name" "it reported a completed sync"
else
    pass "$name"
fi

c="$WORK/empty-path"
mkdir -p "$c/primary" "$c/backup"
echo same >"$c/primary/f"
echo same >"$c/backup/f"
prepare "$c/esp-sync.sh" "$c/primary" "$c/backup"
before=$(tree_state "$c/backup")
EXTRA_PATH="$WORK/findstub" run "$c" "$NVME_P" "$NVME_B"
name="an empty path reaching the stale-file rm aborts"
if [[ $RC -eq 0 ]]; then
    fail "$name" "exit 0, want an abort; stdout: $OUT"
elif [[ "$ERR" != *"filepath: parameter null or not set"* ]]; then
    fail "$name" "exit $RC without the parameter-null abort; stderr: $ERR"
elif [[ "$(tree_state "$c/backup")" != "$before" ]]; then
    fail "$name" "the backup tree changed"
elif [[ "$OUT" == *"ESP sync complete"* ]]; then
    fail "$name" "it reported a completed sync"
else
    pass "$name"
fi

echo
if ((fails)); then
    echo "test_esp_sync: $fails FAILED"
    exit 1
fi
echo "test_esp_sync: all passed"
