#!/bin/bash
# shellcheck disable=SC2034,SC2329
# SC2034: the run state set here is read by the function extracted from
#   backup-run.sh, so the use site is invisible to shellcheck.
# SC2329: the stubs are called by that same extracted code.
#
# tests/test_state_pass.sh — run_state_pass() of scripts/backup-run.sh against
# a stub btrbk: the second, small send of the state subvolume after indexing
# (spec D8). Nothing here mounts or runs anything real.
set -euo pipefail
SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in run_state_pass record_op any_op_is subject_status; do
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
calls() { if [[ -f "$WORK/btrbk.calls" ]]; then wc -l <"$WORK/btrbk.calls"; else echo 0; fi; }
log_info() { echo "INFO $*" >>"$WORK/log"; }
log_warn() { echo "WARN $*" >>"$WORK/log"; }
log_error() { echo "ERROR $*" >>"$WORK/log"; }
declare -A OP_STATUS=() TARGET_MOUNTS=() TARGET_ROLES=()
DAS_BTRBK_CONF="$WORK/btrbk.conf"
: >"$DAS_BTRBK_CONF"
mkdir -p "$WORK/bin"
export PATH="$WORK/bin:$PATH"
export STUB="$WORK"
cat >"$WORK/bin/btrbk" <<'STUBEOF'
#!/bin/bash
echo "$*" >>"$STUB/btrbk.calls"
if [[ -f "$STUB/btrbk.fails" ]]; then
    echo "ERROR: send failed" >&2
    exit 1
fi
echo "sent 12345678 bytes"
STUBEOF
chmod +x "$WORK/bin/btrbk"
fixture() {
    rm -f "$WORK/btrbk.calls" "$WORK/btrbk.fails" "$WORK/log"
    OP_STATUS=()
    TARGET_MOUNTS=([primary-22tb]=/mnt/backup-22tb [system-recovery-A-2tb]=/mnt/backup-system-recovery-A [system-recovery-B-2tb]=/mnt/backup-system-recovery-B)
    TARGET_ROLES=([primary-22tb]=primary [system-recovery-A-2tb]=mirror [system-recovery-B-2tb]=mirror)
    MOUNTED=(/mnt/backup-22tb /mnt/backup-system-recovery-A /mnt/backup-system-recovery-B)
}
mountpoint() {
    local p="${*: -1}" m
    for m in "${MOUNTED[@]}"; do [[ "$m" == "$p" ]] && return 0; done
    return 1
}

echo "--- every mounted target gets the filter, one btrbk call"
fixture
run_state_pass run
check "state pass: OK" "${OP_STATUS[state]}" "OK"
check "state pass: one call" "$(calls)" "1"
all="$(cat "$WORK/btrbk.calls")"
has "state pass: primary filter" "$all" "/mnt/backup-22tb/nvme/das-state"
has "state pass: A filter" "$all" "/mnt/backup-system-recovery-A/nvme/das-state"
has "state pass: B filter" "$all" "/mnt/backup-system-recovery-B/nvme/das-state"
has "state pass: run, not dryrun" "$all" " run "
has "state pass: the script's btrbk.conf" "$all" "-c $DAS_BTRBK_CONF"

echo "--- an absent target is left out, the pass still runs (review focus 3)"
fixture
MOUNTED=(/mnt/backup-22tb /mnt/backup-system-recovery-B)
run_state_pass run
check "absent target: OK" "${OP_STATUS[state]}" "OK"
all="$(cat "$WORK/btrbk.calls")"
has "absent target: B still sent" "$all" "/mnt/backup-system-recovery-B/nvme/das-state"
if [[ "$all" == *"/mnt/backup-system-recovery-A/"* ]]; then fail "absent target: A was passed to btrbk"; else pass "absent target: A not passed"; fi

echo "--- no target mounted: skipped, said"
fixture
MOUNTED=()
run_state_pass run
check "nothing mounted: SKIP" "${OP_STATUS[state]}" "SKIP"
has "nothing mounted: why" "${OP_STATUS[state_detail]}" "no target mounted"
check "nothing mounted: btrbk never called" "$(calls)" "0"

echo "--- btrbk fails: WARN with its words, never OK, and the run still reads as warned"
fixture
touch "$WORK/btrbk.fails"
run_state_pass run
check "btrbk fails: WARN" "${OP_STATUS[state]}" "WARN"
has "btrbk fails: says why" "${OP_STATUS[state_detail]}" "send failed"
has "btrbk fails: logged" "$(cat "$WORK/log")" "WARN"
check "btrbk fails: subject says warnings" "$(subject_status)" "SUCCESS WITH WARNINGS"

echo "--- dry run: nothing sent, said"
fixture
run_state_pass dryrun
check "dry run: SKIP" "${OP_STATUS[state]}" "SKIP"
check "dry run: btrbk never called" "$(calls)" "0"

echo
echo "$passes passed, $fails failed"
((fails == 0)) || exit 1
echo "STATE-PASS SUITE GREEN"
