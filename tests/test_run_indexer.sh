#!/bin/bash
# shellcheck disable=SC2034,SC2329
# SC2034: the run state set here is read by the function extracted from
#   backup-run.sh, so the use site is invisible to shellcheck.
# SC2329: the stubs are called by that same extracted code.
#
# tests/test_run_indexer.sh — run_indexer() of scripts/backup-run.sh against a
# stub btrdasd: the primary walk, then one presence walk per other mounted
# target. Nothing here mounts or runs anything real.
set -euo pipefail
SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in run_indexer record_op any_op_is subject_status; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done
passes=0
fails=0
pass() { passes=$((passes + 1)); echo "ok   $1"; }
fail() { fails=$((fails + 1)); echo "FAIL $1"; }
check() { if [[ "$2" == "$3" ]]; then pass "$1"; else fail "$1: got '$2', want '$3'"; fi; }
has() { if [[ "$2" == *"$3"* ]]; then pass "$1"; else fail "$1: '$3' not in: $2"; fi; }
calls() { if [[ -f "$WORK/walk.calls" ]]; then wc -l <"$WORK/walk.calls"; else echo 0; fi; }
log_info() { echo "INFO $*" >>"$WORK/log"; }
log_warn() { echo "WARN $*" >>"$WORK/log"; }
declare -A OP_STATUS=() TARGET_MOUNTS=() TARGET_ROLES=()
DAS_DB_PATH="$WORK/index.db"
BTRDASD_BIN="$WORK/bin/btrdasd"
mkdir -p "$WORK/bin"
# Logs its args and the lock-fd variable; fails for any root named in walk.fails.
cat >"$WORK/bin/btrdasd" <<'STUBEOF'
#!/bin/bash
echo "$* fd=${DAS_MAINTENANCE_LOCK_FD:-unset}" >>"$STUB_DIR/walk.calls"
if [[ -f "$STUB_DIR/walk.fails" ]] && grep -qxF -- "$2" "$STUB_DIR/walk.fails"; then
    echo "walk exploded on $2" >&2
    exit 7
fi
echo "walked $2"
STUBEOF
chmod +x "$WORK/bin/btrdasd"
export STUB_DIR="$WORK"
mountpoint() {
    local p="${*: -1}" m
    for m in "${MOUNTED[@]}"; do [[ "$m" == "$p" ]] && return 0; done
    return 1
}
fixture() {
    rm -f "$WORK/walk.calls" "$WORK/walk.fails" "$WORK/log"
    OP_STATUS=()
    TARGET_MOUNTS=([primary-22tb]=/mnt/backup-22tb [system-recovery-A-2tb]=/mnt/backup-system-recovery-A [system-recovery-B-2tb]=/mnt/backup-system-recovery-B)
    TARGET_ROLES=([primary-22tb]=primary [system-recovery-A-2tb]=mirror [system-recovery-B-2tb]=mirror)
    MOUNTED=(/mnt/backup-22tb /mnt/backup-system-recovery-A /mnt/backup-system-recovery-B)
}

echo "--- primary only mounted: one walk"
fixture
MOUNTED=(/mnt/backup-22tb)
run_indexer
check "primary only: OK" "${OP_STATUS[indexer]}" "OK"
check "primary only: one walk" "$(calls)" "1"
has "primary only: the primary" "$(cat "$WORK/walk.calls")" "walk /mnt/backup-22tb --db $DAS_DB_PATH fd=8"
check "primary only: no presence claimed" "${OP_STATUS[indexer_detail]:-}" ""

echo "--- primary + two mirrors: three walks, primary first, presence names both"
fixture
run_indexer
check "three: OK" "${OP_STATUS[indexer]}" "OK"
check "three: three walks" "$(calls)" "3"
first="$(head -n1 "$WORK/walk.calls")"
has "three: primary first" "$first" "walk /mnt/backup-22tb "
all="$(cat "$WORK/walk.calls")"
has "three: A walked" "$all" "walk /mnt/backup-system-recovery-A --db $DAS_DB_PATH fd=8"
has "three: B walked" "$all" "walk /mnt/backup-system-recovery-B --db $DAS_DB_PATH fd=8"
check "three: presence detail" "${OP_STATUS[indexer_detail]}" "presence: system-recovery-A-2tb, system-recovery-B-2tb"
has "three: logged" "$(cat "$WORK/log")" "presence recorded on system-recovery-A-2tb"

echo "--- an unmounted mirror is not walked"
fixture
MOUNTED=(/mnt/backup-22tb /mnt/backup-system-recovery-B)
run_indexer
check "unmounted mirror: two walks" "$(calls)" "2"
check "unmounted mirror: detail" "${OP_STATUS[indexer_detail]}" "presence: system-recovery-B-2tb"

echo "--- a mirror walk fails: WARN naming it, the other mirror still walked, not FAIL"
fixture
echo /mnt/backup-system-recovery-A >"$WORK/walk.fails"
run_indexer
check "mirror fails: WARN" "${OP_STATUS[indexer]}" "WARN"
check "mirror fails: three walks" "$(calls)" "3"
has "mirror fails: names the label" "${OP_STATUS[indexer_detail]}" "system-recovery-A-2tb: failed"
has "mirror fails: its words" "${OP_STATUS[indexer_detail]}" "walk exploded"
has "mirror fails: B still recorded" "${OP_STATUS[indexer_detail]}" "presence: system-recovery-B-2tb"
has "mirror fails: logged" "$(cat "$WORK/log")" "WARN Presence walk of system-recovery-A-2tb failed"
check "mirror fails: subject warns, not failure" "$(subject_status)" "SUCCESS WITH WARNINGS"

echo "--- the primary walk fails: FAIL as before, and NO mirror walks"
fixture
echo /mnt/backup-22tb >"$WORK/walk.fails"
run_indexer
check "primary fails: FAIL" "${OP_STATUS[indexer]}" "FAIL"
has "primary fails: exit code" "${OP_STATUS[indexer_detail]}" "exit code 7"
check "primary fails: only the primary tried" "$(calls)" "1"

echo "--- primary not mounted: SKIP, no walks"
fixture
MOUNTED=()
run_indexer
check "unmounted primary: SKIP" "${OP_STATUS[indexer]}" "SKIP"
check "unmounted primary: no walks" "$(calls)" "0"

echo
echo "$passes passed, $fails failed"
((fails == 0)) || exit 1
echo "RUN-INDEXER SUITE GREEN"
