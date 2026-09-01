#!/bin/bash
# shellcheck disable=SC2034,SC2329
# SC2034: the associative arrays and STUB_* fixtures are read at runtime by the
#   function extracted from backup-run.sh, so the use site cannot be seen here.
# SC2329: the log_*/record_op/mountpoint/findmnt/blkid stubs are called by that
#   same extracted code.
# tests/test_verify_sources.sh
#
# Regression test for DAS-Backup-Manager-zlv.
#
# verify_targets_before_btrbk() has proved since bd DAS-Backup-Manager-9on that
# every TARGET is a real mountpoint carrying the expected filesystem. SOURCES
# had no equivalent: mount_sources() runs `mountpoint -q`, mounts if absent,
# logs success, and nothing checks WHAT got mounted. An empty
# /.btrfs-hdd/.btrbk-snapshots directory dated 2026-05-17 was found on the NVMe
# ROOT filesystem — create_snapshot_dirs() had run against a bare mountpoint.
#
# v4.5.0 adds verify_sources_before_write(). This test extracts it from the
# live backup-run.sh by `sed`, stubs mountpoint/findmnt/blkid as shell
# functions driven by fixture tables, and asserts BOTH directions:
#
#   - it ACCEPTS correctly-mounted sources (the positive control — a guard that
#     refused everything would fail case 1 and case 7)
#   - it REFUSES a bare mountpoint, a wrong filesystem, an unresolvable device,
#     a missing device, a findmnt that answers nothing, and a mount rooted at a
#     subvolume rather than the top-level volume
#
# Case 3 is the one that matters most: it reproduces the real /.btrfs-nvme
# geometry where the source filesystem and the root filesystem are the SAME
# filesystem, so `findmnt --target` on a BARE mountpoint returns a MATCHING
# UUID. Only the `mountpoint -q` check can refuse it. If someone later
# "simplifies" the guard down to the UUID comparison, case 3 goes red.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/backup-run.sh"

if [[ ! -f "$SCRIPT" ]]; then
    echo "FAIL: cannot find $SCRIPT" >&2
    exit 1
fi

# ---------------------------------------------------------------- fixtures --
# Real UUIDs from this host, so the geometry under test is the real geometry:
#   ROOT_FS_UUID  the NVMe root filesystem — AND the `nvme` source's own
#                 filesystem, reached at /.btrfs-nvme as its subvolid=5 view
#   HDD_FS_UUID   /hddRaid1 / /.btrfs-hdd
#   SSD_FS_UUID   /.btrfs-ssd
ROOT_FS_UUID="20b5fa7e-d8c0-4035-ae45-f80263073a96"
HDD_FS_UUID="8b66e847-4273-4e2a-ad53-b312b3b3ee6d"
SSD_FS_UUID="2638d087-0be1-436e-bfe4-8d6551ec02be"
OTHER_FS_UUID="ffffffff-0000-0000-0000-ffffffffffff"

declare -A SOURCE_VOLUMES=()
declare -A SOURCE_DEVICES=()
declare -A OP_STATUS=()

# Stub tables, all keyed by path/device.
declare -A STUB_MOUNTED=()     # path -> "1" when `mountpoint -q` should succeed
declare -A STUB_FINDMNT=()     # path -> "<uuid> <fsroot>"; unset => findmnt fails
declare -A STUB_BLKID=()       # device -> uuid; unset => blkid fails

log()       { :; }
log_info()  { echo "[INFO] $1"; }
log_warn()  { echo "[WARN] $1"; }
log_error() { echo "[ERROR] $1"; }

record_op() {
    local op="$1" result="$2" detail="${3:-}"
    OP_STATUS[$op]="$result"
    if [[ -n "$detail" ]]; then
        OP_STATUS["${op}_detail"]="$detail"
    fi
}

# The guard calls: mountpoint -q "$mnt"
mountpoint() {
    local p="${!#}"
    [[ -n "${STUB_MOUNTED[$p]:-}" ]]
}

# The guard calls: findmnt -n -o UUID,FSROOT --target "$mnt"
findmnt() {
    local p="${!#}"
    local v="${STUB_FINDMNT[$p]:-}"
    [[ -n "$v" ]] || return 1
    echo "$v"
}

# The guard calls: blkid -o value -s UUID "$dev"
blkid() {
    local d="${!#}"
    local v="${STUB_BLKID[$d]:-}"
    [[ -n "$v" ]] || return 2
    echo "$v"
}

# Source the function out of the live script.
eval "$(sed -n '/^verify_sources_before_write() {/,/^}/p' "$SCRIPT")"

if ! declare -F verify_sources_before_write >/dev/null; then
    echo "FAIL: verify_sources_before_write() not found in $SCRIPT" >&2
    exit 1
fi

# ------------------------------------------------------------------ harness --
FAIL=0
CASE_NO=0

reset_fixtures() {
    SOURCE_VOLUMES=(); SOURCE_DEVICES=(); OP_STATUS=()
    STUB_MOUNTED=(); STUB_FINDMNT=(); STUB_BLKID=()
}

# Stage the healthy baseline: three source volumes, seven labels, exactly the
# shape of the live config (several labels share one volume, and `nvme` uses a
# /dev path while the rest use UUID= form).
stage_healthy() {
    reset_fixtures

    SOURCE_VOLUMES[nvme]="/.btrfs-nvme";     SOURCE_DEVICES[nvme]="/dev/nvme1n1p2"
    SOURCE_VOLUMES[nvme-vm]="/.btrfs-nvme";  SOURCE_DEVICES[nvme-vm]="/dev/nvme1n1p2"
    SOURCE_VOLUMES[ssd]="/.btrfs-ssd";       SOURCE_DEVICES[ssd]="UUID=$SSD_FS_UUID"
    SOURCE_VOLUMES[ssd-vm]="/.btrfs-ssd";    SOURCE_DEVICES[ssd-vm]="UUID=$SSD_FS_UUID"
    SOURCE_VOLUMES[hdd-projects]="/.btrfs-hdd"; SOURCE_DEVICES[hdd-projects]="UUID=$HDD_FS_UUID"
    SOURCE_VOLUMES[hdd-media]="/.btrfs-hdd";    SOURCE_DEVICES[hdd-media]="UUID=$HDD_FS_UUID"

    STUB_MOUNTED[/.btrfs-nvme]=1
    STUB_MOUNTED[/.btrfs-ssd]=1
    STUB_MOUNTED[/.btrfs-hdd]=1

    STUB_FINDMNT[/.btrfs-nvme]="$ROOT_FS_UUID /"
    STUB_FINDMNT[/.btrfs-ssd]="$SSD_FS_UUID /"
    STUB_FINDMNT[/.btrfs-hdd]="$HDD_FS_UUID /"

    STUB_BLKID[/dev/nvme1n1p2]="$ROOT_FS_UUID"
}

# Run the guard in a subshell so its `exit 1` cannot kill the harness.
# Prints "rc=<n>" plus the captured output for the caller to assert on.
run_guard() {
    local out rc=0
    out=$( verify_sources_before_write 2>&1 ) || rc=$?
    LAST_OUT="$out"
    LAST_RC="$rc"
}

expect_accept() {
    local name="$1"
    CASE_NO=$(( CASE_NO + 1 ))
    ( run_guard; [[ "$LAST_RC" -eq 0 ]] && grep -q 'All source volumes verified' <<< "$LAST_OUT" ) \
        && { echo "  PASS [case $CASE_NO] ACCEPT: $name"; return 0; }
    echo "  FAIL [case $CASE_NO] ACCEPT: $name — guard refused a healthy source set"
    ( run_guard; printf '%s\n' "$LAST_OUT" | sed 's/^/        /' ) || true
    FAIL=1
}

expect_refuse() {
    local name="$1" expect_substr="$2"
    CASE_NO=$(( CASE_NO + 1 ))
    local out rc=0
    out=$( verify_sources_before_write 2>&1 ) || rc=$?
    if [[ "$rc" -eq 0 ]]; then
        echo "  FAIL [case $CASE_NO] REFUSE: $name — guard ACCEPTED (exit 0) what it must refuse"
        printf '%s\n' "$out" | sed 's/^/        /'
        FAIL=1
        return 0
    fi
    if ! grep -q 'ABORTING' <<< "$out"; then
        echo "  FAIL [case $CASE_NO] REFUSE: $name — nonzero exit but no ABORTING banner"
        FAIL=1
        return 0
    fi
    if ! grep -qF -- "$expect_substr" <<< "$out"; then
        echo "  FAIL [case $CASE_NO] REFUSE: $name — refused, but not for the expected reason"
        echo "        expected to contain: $expect_substr"
        printf '%s\n' "$out" | sed 's/^/        /'
        FAIL=1
        return 0
    fi
    echo "  PASS [case $CASE_NO] REFUSE: $name"
}

echo "=== verify_sources_before_write() — both directions ==="
echo

# ---- case 1: POSITIVE CONTROL. A guard that refused everything would fail
#      here, so cases 2-7 cannot be satisfied by a blanket refusal.
stage_healthy
expect_accept "all sources mounted, correct UUIDs, top-level volume"

# ---- case 2: POSITIVE CONTROL (narrow). Single UUID= source, nothing else in
#      play — proves the accept path is not an accident of the larger fixture.
reset_fixtures
SOURCE_VOLUMES[hdd-projects]="/.btrfs-hdd"
SOURCE_DEVICES[hdd-projects]="UUID=$HDD_FS_UUID"
STUB_MOUNTED[/.btrfs-hdd]=1
STUB_FINDMNT[/.btrfs-hdd]="$HDD_FS_UUID /"
expect_accept "single UUID= source, correctly mounted"

# ---- case 3: the observed defect. /.btrfs-hdd bare; findmnt --target falls
#      through to the ROOT filesystem, which is a DIFFERENT uuid here, so both
#      checks would catch it. This is the 2026-05-17 artifact's shape.
stage_healthy
unset 'STUB_MOUNTED[/.btrfs-hdd]'
STUB_FINDMNT[/.btrfs-hdd]="$ROOT_FS_UUID /@"
expect_refuse "bare /.btrfs-hdd (2026-05-17 artifact shape)" \
    "/.btrfs-hdd is NOT a mountpoint"

# ---- case 4: THE case the UUID check cannot see. /.btrfs-nvme bare; the nvme
#      source's own filesystem IS the root filesystem, so findmnt --target on
#      the bare path returns a MATCHING uuid. Measured on this host:
#        findmnt -n -o UUID --target /.btrfs-nvme  ->  20b5fa7e-...
#      with /.btrfs-nvme demonstrably not a mountpoint. Only `mountpoint -q`
#      refuses this. Delete that check and this case goes red while case 3
#      stays green.
stage_healthy
unset 'STUB_MOUNTED[/.btrfs-nvme]'
STUB_FINDMNT[/.btrfs-nvme]="$ROOT_FS_UUID /@"
expect_refuse "bare /.btrfs-nvme where source fs == root fs (UUID check is blind here)" \
    "/.btrfs-nvme is NOT a mountpoint"

# ---- case 4b: ISOLATES the `mountpoint -q` check. A bare source path nested
#      under an already-mounted top-level volume of the SAME filesystem — this
#      host has exactly that geometry available (/dasRaid0 is mounted with
#      FSROOT '/' and UUID d29fdda7-…, so any bare path beneath it answers with
#      a MATCHING uuid AND a MATCHING '/' fsroot). Both identity checks say
#      "fine"; only `mountpoint -q` refuses. Case 4's fixture is also caught by
#      the FSROOT check (the real /.btrfs-nvme falls through to '/', whose
#      fsroot is '/@'), so this case — not case 4 — is the one that goes red
#      alone if the mountpoint check is deleted.
reset_fixtures
DAS_FS_UUID="d29fdda7-a1e5-4640-996e-2b78569cb65d"
SOURCE_VOLUMES[das-storage]="/dasRaid0/nested-source"
SOURCE_DEVICES[das-storage]="UUID=$DAS_FS_UUID"
# NOT in STUB_MOUNTED -> bare. findmnt --target falls through to /dasRaid0.
STUB_FINDMNT[/dasRaid0/nested-source]="$DAS_FS_UUID /"
expect_refuse "bare path whose fallthrough matches BOTH uuid and fsroot" \
    "/dasRaid0/nested-source is NOT a mountpoint"

# ---- case 5: mounted, but it is a different filesystem than configured.
stage_healthy
STUB_FINDMNT[/.btrfs-hdd]="$OTHER_FS_UUID /"
expect_refuse "wrong filesystem mounted at /.btrfs-hdd" \
    "a different filesystem is mounted here"

# ---- case 6: fail CLOSED — device path cannot be resolved to a UUID.
stage_healthy
unset 'STUB_BLKID[/dev/nvme1n1p2]'
expect_refuse "unresolvable /dev path (blkid returns nothing) fails closed" \
    "no resolvable filesystem UUID"

# ---- case 7: fail CLOSED — no device configured at all.
stage_healthy
SOURCE_DEVICES[hdd-media]=""
expect_refuse "source with empty device field fails closed" \
    "no device configured"

# ---- case 8: fail CLOSED — mountpoint -q says yes but findmnt answers nothing.
stage_healthy
unset 'STUB_FINDMNT[/.btrfs-ssd]'
expect_refuse "findmnt reporting nothing fails closed" \
    "findmnt could not report its UUID/FSROOT"

# ---- case 9: right filesystem, wrong root — mounted at a subvolume instead of
#      the top-level volume mount_sources() asks for with -o subvolid=5.
stage_healthy
STUB_FINDMNT[/.btrfs-hdd]="$HDD_FS_UUID /@"
expect_refuse "correct UUID but mounted at subvolume '/@' instead of top level" \
    "mounted at subvolume '/@'"

# ---- case 10: no volume path configured at all.
reset_fixtures
SOURCE_VOLUMES[broken]=""
SOURCE_DEVICES[broken]="UUID=$HDD_FS_UUID"
expect_refuse "source with empty volume path fails closed" \
    "no volume path configured"

# ---- case 11: OP_STATUS bookkeeping on the accept path (record_op wiring).
stage_healthy
OP_STATUS=()
CASE_NO=$(( CASE_NO + 1 ))
if verify_sources_before_write >/dev/null 2>&1 && [[ "${OP_STATUS[verify_sources]:-}" == "OK" ]]; then
    echo "  PASS [case $CASE_NO] record_op: OP_STATUS[verify_sources]=OK after a clean pass"
else
    echo "  FAIL [case $CASE_NO] record_op: expected OP_STATUS[verify_sources]=OK, got '${OP_STATUS[verify_sources]:-<unset>}'"
    FAIL=1
fi

echo
if [[ $FAIL -eq 0 ]]; then
    echo "OK — verify_sources_before_write accepts healthy sources and refuses"
    echo "     bare mountpoints, wrong filesystems, unverifiable devices and"
    echo "     non-top-level mounts. ($CASE_NO cases)"
else
    echo "FAILED — verify_sources_before_write regression." >&2
fi
exit $FAIL
