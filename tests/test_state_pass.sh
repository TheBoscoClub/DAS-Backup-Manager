#!/bin/bash
# shellcheck disable=SC2016
# SC2016: the step-order checks match the literal text `run_state_pass "$mode"`
#   and `if [[ "$mode" != "dryrun" ]]` in the script's source; expanding them
#   would defeat the match.
# shellcheck disable=SC2034,SC2329
# SC2034: the run state set here is read by the function extracted from
#   backup-run.sh, so the use site is invisible to shellcheck.
# SC2329: the stubs are called by that same extracted code.
#
# tests/test_state_pass.sh — run_state_pass() of scripts/backup-run.sh against
# a stub btrbk: the second, small send of the state subvolume after indexing
# (spec D8). Nothing here mounts or runs anything real.
set -euo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"
SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in run_state_pass record_op any_op_is subject_status format_bytes; do
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
cat >"$WORK/bin/btrfs" <<'STUBEOF'
#!/bin/bash
echo "$*" >>"$STUB/btrfs.calls"
[[ -f "$STUB/btrfs.fails" ]] && exit 1
exit 0
STUBEOF
chmod +x "$WORK/bin/btrfs"
# A realistic rendered btrbk.conf: the volume block's target lines, then the
# das-state entry (and a neighbour), as `btrdasd setup` renders them.
write_conf() {
    cat >"$1" <<CONFEOF
# nvme
volume /.btrfs-nvme
  snapshot_dir          .btrbk-snapshots
  target                /mnt/backup-22tb/nvme
  target                /mnt/backup-system-recovery-B/nvme
    target_preserve     7d
  target                /mnt/backup-system-recovery-A/nvme
    target_preserve     7d
  target                /mnt/unconfigured/nvme

  subvolume             @
    snapshot_name       root-

  subvolume             @/var/lib/das-backup
    snapshot_name       das-state
CONFEOF
}
# Used space on a target: 10 MB, plus 1 MiB per btrbk call made so far, so a
# send "grows" each mounted target by 1 MiB. gu.fails makes the reading fail.
get_used_bytes() {
    [[ -f "$WORK/gu.fails" ]] && return 1
    if [[ -f "$WORK/gu.shrinks" ]]; then echo $((100000000 - $(calls) * 1048576)); return; fi
    echo $((10000000 + $(calls) * 1048576))
}
fixture() {
    write_conf "$DAS_BTRBK_CONF"
    unset DAS_BTRBK_STATE_CONF
    rm -f "$WORK/gu.fails" "$WORK/gu.shrinks" "$WORK/btrfs.calls" "$WORK/btrfs.fails"
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
has "state pass: bytes in the detail" "${OP_STATUS[state_detail]}" "3 target(s), 3.00 MiB total"
if [[ "$all" == *"/mnt/unconfigured"* ]]; then fail "unconfigured target line passed to btrbk"; else pass "unconfigured target line ignored"; fi
has "unconfigured target line: logged" "$(cat "$WORK/log")" "belongs to no configured mount"

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

echo "--- a size that cannot be read is said, never 0"
fixture
touch "$WORK/gu.fails"
run_state_pass run
check "unreadable size: still OK" "${OP_STATUS[state]}" "OK"
has "unreadable size: bytes unknown" "${OP_STATUS[state_detail]}" "bytes unknown"

echo "--- used space that FELL is not a size: bytes unknown, never 0 B"
fixture
touch "$WORK/gu.shrinks"
run_state_pass run
check "shrunk: still OK" "${OP_STATUS[state]}" "OK"
has "shrunk: bytes unknown" "${OP_STATUS[state_detail]}" "bytes unknown"
if [[ "${OP_STATUS[state_detail]}" == *"0 B"* ]]; then fail "shrunk: a made-up 0 B"; else pass "shrunk: no made-up 0 B"; fi

echo "--- the second reading follows a btrfs filesystem sync of each mounted target"
fixture
run_state_pass run
sync_calls="$(cat "$WORK/btrfs.calls" 2>/dev/null || true)"
has "sync: primary" "$sync_calls" "filesystem sync /mnt/backup-22tb"
has "sync: A" "$sync_calls" "filesystem sync /mnt/backup-system-recovery-A"
has "sync: B" "$sync_calls" "filesystem sync /mnt/backup-system-recovery-B"
fixture
touch "$WORK/btrfs.fails"
run_state_pass run
check "sync fails: still OK" "${OP_STATUS[state]}" "OK"
has "sync fails: bytes unknown" "${OP_STATUS[state_detail]}" "bytes unknown"

echo "--- a das-state entry with no target lines says so"
fixture
printf 'volume /.btrfs-nvme\n  snapshot_dir          .btrbk-snapshots\n  subvolume             @/var/lib/das-backup\n    snapshot_name       das-state\n' >"$DAS_BTRBK_CONF"
run_state_pass run
check "no target lines: SKIP" "${OP_STATUS[state]}" "SKIP"
has "no target lines: why" "${OP_STATUS[state_detail]}" "das-state entry declares no target"
check "no target lines: btrbk never called" "$(calls)" "0"

echo "--- no das-state entry in the conf: skipped, btrbk never called"
fixture
printf 'volume /.btrfs-nvme\n  target                /mnt/backup-22tb/nvme\n  subvolume             @\n    snapshot_name       root-\n' >"$DAS_BTRBK_CONF"
run_state_pass run
check "no entry: SKIP" "${OP_STATUS[state]}" "SKIP"
has "no entry: why" "${OP_STATUS[state_detail]}" "no das-state entry in btrbk.conf (the state subvolume is not configured yet)"
check "no entry: btrbk never called" "$(calls)" "0"

echo "--- DAS_BTRBK_STATE_CONF overrides the config the pass reads"
fixture
write_conf "$WORK/state.conf"
: >"$DAS_BTRBK_CONF"
DAS_BTRBK_STATE_CONF="$WORK/state.conf" run_state_pass run
check "state conf: OK" "${OP_STATUS[state]}" "OK"
has "state conf: btrbk -c that file" "$(cat "$WORK/btrbk.calls")" "-c $WORK/state.conf"

echo "--- DAS_BTRBK_STATE_CONF set to an ABSENT file (dump-env always sets it): SKIP, not WARN"
fixture
DAS_BTRBK_STATE_CONF="$WORK/no-such-state.conf" run_state_pass run
check "absent state conf: SKIP" "${OP_STATUS[state]}" "SKIP"
has "absent state conf: why" "${OP_STATUS[state_detail]}" "no das-state entry in btrbk.conf (the state subvolume is not configured yet)"
check "absent state conf: btrbk never called" "$(calls)" "0"
check "absent state conf: subject stays clean" "$(subject_status)" "SUCCESS"

echo "--- DAS_BTRBK_STATE_CONF set to something unreadable (a directory): WARN, still said"
fixture
mkdir -p "$WORK/state-dir"
DAS_BTRBK_STATE_CONF="$WORK/state-dir" run_state_pass run
check "unreadable state conf: WARN" "${OP_STATUS[state]}" "WARN"
has "unreadable state conf: says why" "${OP_STATUS[state_detail]}" "cannot read $WORK/state-dir"
check "unreadable state conf: btrbk never called" "$(calls)" "0"

echo "--- step order in main(): run_state_pass after run_indexer, same non-dryrun branch"
main_txt="$(extract main)"
[[ -n "$main_txt" ]] || { echo "FAIL: main not found"; exit 1; }
ix="$(printf '%s\n' "$main_txt" | grep -n '^        run_indexer$' | cut -d: -f1 || true)"
st="$(printf '%s\n' "$main_txt" | grep -n '^        run_state_pass "\$mode"$' | cut -d: -f1 || true)"
if [[ -n "$ix" && -n "$st" ]] && ((st > ix)); then pass "run_state_pass is after run_indexer"; else fail "order: indexer line '${ix:-none}', state line '${st:-none}'"; fi
ifl="$(printf '%s\n' "$main_txt" | head -n "${ix:-1}" | grep -n '^    if \[\[ "\$mode" != "dryrun" \]\]; then$' | tail -1 | cut -d: -f1 || true)"
closer="$(printf '%s\n' "$main_txt" | awk -v a="${ifl:-0}" 'NR > a && /^    fi$/ { print NR; exit }')"
if [[ -n "$ifl" && -n "$st" && -n "$closer" ]] && ((ifl < ix && st < closer)); then pass "both calls sit in the same non-dryrun branch"; else fail "branch: if ${ifl:-none}, indexer ${ix:-none}, state ${st:-none}, fi ${closer:-none}"; fi

echo "--- dry run: nothing sent, said"
fixture
run_state_pass dryrun
check "dry run: SKIP" "${OP_STATUS[state]}" "SKIP"
check "dry run: btrbk never called" "$(calls)" "0"

echo
echo "$passes passed, $fails failed"
((fails == 0)) || exit 1
echo "STATE-PASS SUITE GREEN"
