#!/bin/bash
# shellcheck disable=SC2016,SC2034,SC2329
# SC2016: the lines extract() looks for are literal text, single-quoted so.
# SC2034, SC2329: the globals and stub functions each case defines are read
#   and called by the function it sourced, which shellcheck cannot see.
# A reader that quits early after a pipe, under `set -o pipefail`
# (bd DAS-Backup-Manager-wkvz).
#
#   if echo "$big" | grep -q PATTERN; then ...
#
# grep -q exits at its first match. A producer still writing then dies of
# SIGPIPE (141), pipefail makes the pipeline 141, and the `if` reads that as
# "no match". It takes input larger than a pipe (64 KiB) with a match before
# its end — several lines, since grep reads to a line's end before it matches
# (a single 70 KB line matched 100 of 100) — and then it happens every time
# (measured: 141 in 100 of 100).
#
# Each of the four sites runs here as its script runs it: the real function,
# extracted from the script, under the script's own `set -euo pipefail`, on
# input over 64 KiB that matches on its FIRST line. Each must take the
# matching branch. Every branch also runs on small input, so the fix is shown
# to change nothing else.
#
#   backup-run.sh          update_boot_subvolumes  the drift check on a
#                          target's subvolume listing (exposed: the primary
#                          target's listing is near 64 KiB and grows)
#   backup-verify.sh       check_smart_health      the SMART health line
#   das-partition-drives.sh check_smart_tests      both self-test lines
#
# The last two are latent. smartctl -H prints one health line, and the
# self-test status is cut to one line by `head -1` before it is tested, so
# their own input never reaches 64 KiB in several lines. The large cases hand
# the conditions the input they would need: for check_smart_tests by letting
# `head` pass every line through.
#
# Writes only beneath a mktemp directory. No root, no devices.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)" && [[ -d "$WORK" ]] || {
    echo "HARNESS BROKEN: no temp dir"
    exit 2
}
trap 'rm -rf "${WORK:?}"' EXIT

pass=0
fail=0
ok() {
    echo "ok   $1"
    pass=$((pass + 1))
}
bad() {
    echo "FAIL $1"
    fail=$((fail + 1))
}
check() { # check <name> <got> <want>
    if [[ "$2" == "$3" ]]; then
        ok "$1"
    else
        bad "$1 — got '$2', want '$3'"
    fi
}
harness_broken() {
    echo "HARNESS BROKEN: $*"
    exit 2
}

# extract <script> <function> <a line the function must contain>: the
# function's text, into $WORK/<function>.sh. A range that caught nothing, or
# caught a function without the line under test, stops the suite.
extract() {
    local out="$WORK/$2.sh"
    sed -n "/^$2() {/,/^}/p" "$ROOT/scripts/$1" >"$out"
    [[ -s "$out" ]] || harness_broken "$2 not found in $1"
    grep -qF -- "$3" "$out" || harness_broken "$2 in $1 no longer contains: $3"
}

# A file of <n> filler lines that match nothing below: no 8-digit date, and
# none of the words the conditions look for.
filler() { # filler <lines>
    local i
    for ((i = 0; i < $1; i++)); do
        printf 'ID %06d gen 4242 top level 5 path data/plain-subvolume-%06d\n' "$i" "$i"
    done
}
big_size_ok() { # big_size_ok <file>: over 64 KiB, or the case proves nothing
    local n
    n="$(wc -c <"$1")"
    ((n > 65536)) || harness_broken "$1 is $n bytes, not over 64 KiB"
}

# ---------------------------------------------------------------------------
echo "== backup-run.sh: update_boot_subvolumes, the drift check"
# ---------------------------------------------------------------------------
# A target whose listing HAS btrbk-shaped snapshots that match neither name
# pattern is a drift, a FAIL; one with none is a quiet skip.
extract backup-run.sh update_boot_subvolumes "Target HAS btrbk-shaped snapshots but none matched"
extract backup-run.sh record_op 'OP_STATUS[$op]="$result"'

run_boot_subvols() { # run_boot_subvols <listing file>: "<result>|<detail>"
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/update_boot_subvolumes.sh"
        # shellcheck source=/dev/null
        source "$WORK/record_op.sh"
        LISTING="$1"
        declare -A OP_STATUS=()
        declare -A MOUNT_ROLES=([/mnt/t]=primary)
        ALL_TARGET_MOUNTS=(/mnt/t)
        mountpoint() { return 0; }
        btrfs() {
            case "$1 $2" in
            "filesystem label") echo das-backup-test ;;
            "subvolume list") cat "$LISTING" ;;
            *)
                echo "btrfs stub: unexpected: $*" >&2
                return 99
                ;;
            esac
        }
        log_info() { echo "[INFO] $*"; }
        log_warn() { echo "[WARN] $*"; }
        log_error() { echo "[ERROR] $*"; }
        update_boot_subvolumes true >"$WORK/boot.out" 2>&1
        printf '%s|%s\n' "${OP_STATUS[boot_subvols]:-unset}" "${OP_STATUS[boot_subvols_detail]:-}"
    )
}
DRIFTED='ID 300 gen 9 top level 5 path nvme/renamed-root.20261004T0300'

{
    echo "$DRIFTED"
    filler 1500
} >"$WORK/drift-big.txt"
big_size_ok "$WORK/drift-big.txt"
check "boot subvolumes, drift on line 1 of a listing over 64 KiB: FAIL" \
    "$(run_boot_subvols "$WORK/drift-big.txt")" "FAIL|0 updated, 1 failed"
check "boot subvolumes, drift over 64 KiB: says it drifted" \
    "$(grep -c 'HAS btrbk-shaped snapshots but none matched' "$WORK/boot.out")" "1"

echo "$DRIFTED" >"$WORK/drift-small.txt"
check "boot subvolumes, drift in a small listing: FAIL" \
    "$(run_boot_subvols "$WORK/drift-small.txt")" "FAIL|0 updated, 1 failed"

filler 1500 >"$WORK/none-big.txt"
big_size_ok "$WORK/none-big.txt"
check "boot subvolumes, no btrbk snapshots in a listing over 64 KiB: the quiet skip" \
    "$(run_boot_subvols "$WORK/none-big.txt")" "OK|0 updated, 1 skipped"
check "boot subvolumes, no btrbk snapshots: says it skips" \
    "$(grep -c 'No btrbk snapshots found, skipping' "$WORK/boot.out")" "1"

# ---------------------------------------------------------------------------
echo "== backup-verify.sh: check_smart_health, the health line"
# ---------------------------------------------------------------------------
extract backup-verify.sh check_smart_health 'all_passed=false'

run_health() { # run_health <smartctl -H output file>: the verdict line
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/check_smart_health.sh"
        HEALTH_OUT="$1"
        RED="" GREEN="" YELLOW="" BLUE="" NC=""
        DAS_DEVICES=(/dev/sdz)
        declare -A DRIVE_MAP=([TESTSERIAL]="test drive")
        SMART_FAILED=false
        smartctl() {
            case "$1" in
            -H) cat "$HEALTH_OUT" ;;
            -l) echo "# 1  Short offline       Completed without error       00%      1000         -" ;;
            *) return 0 ;;
            esac
        }
        read_drive_serial() { echo TESTSERIAL; }
        smart_attr_raw() { echo 0; }
        report_sector_attr() { return 0; }
        format_attr() { echo "$1${2:-}"; }
        log_header() { :; }
        log_info() { echo "[INFO] $*"; }
        log_warn() { echo "[WARN] $*"; }
        check_smart_health >"$WORK/health.out" 2>&1
        if [[ "$SMART_FAILED" == true ]]; then echo "issues"; else echo "passed"; fi
    )
}
PASSED_LINE='SMART overall-health self-assessment test result: PASSED'

{
    echo "$PASSED_LINE"
    for ((i = 0; i < 1500; i++)); do
        printf 'SMART overall-health self-assessment test result: line %06d\n' "$i"
    done
} >"$WORK/health-big.txt"
big_size_ok "$WORK/health-big.txt"
check "SMART health, PASSED on line 1 of output over 64 KiB: passed" \
    "$(run_health "$WORK/health-big.txt")" "passed"
check "SMART health, over 64 KiB: the PASSED line" \
    "$(grep -c '^  Health: PASSED$' "$WORK/health.out")" "1"

echo "$PASSED_LINE" >"$WORK/health-passed.txt"
check "SMART health, one PASSED line: passed" "$(run_health "$WORK/health-passed.txt")" "passed"
echo 'SMART overall-health self-assessment test result: FAILED!' >"$WORK/health-failed.txt"
check "SMART health, one FAILED line: issues" "$(run_health "$WORK/health-failed.txt")" "issues"
: >"$WORK/health-none.txt"
check "SMART health, no health line: issues" "$(run_health "$WORK/health-none.txt")" "issues"

# ---------------------------------------------------------------------------
echo "== das-partition-drives.sh: check_smart_tests, the self-test lines"
# ---------------------------------------------------------------------------
extract das-partition-drives.sh check_smart_tests 'all_complete=false'

run_selftest() { # run_selftest <selftest log file> [passthrough]: "<rc>|<what it said>"
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/check_smart_tests.sh"
        SELFTEST_OUT="$1"
        RED="" GREEN="" YELLOW="" BLUE="" NC=""
        declare -A DISCOVERED_DEVICES=([TESTSERIAL]=/dev/sdz)
        declare -A TARGET_LABELS=([TESTSERIAL]=test-target)
        smartctl() { cat "$SELFTEST_OUT"; }
        # The function cuts the status to one line with `head -1`; to hand
        # its conditions the input they would need, head passes every line.
        if [[ "${2:-}" == passthrough ]]; then
            head() { cat; }
        fi
        log_header() { :; }
        log_info() { echo "[INFO] $*"; }
        log_warn() { echo "[WARN] $*"; }
        local_rc=0
        check_smart_tests >"$WORK/selftest.out" 2>&1 || local_rc=$?
        said="other"
        grep -q 'Test still running' "$WORK/selftest.out" && said="running"
        grep -q 'Test completed - PASSED' "$WORK/selftest.out" && said="completed"
        echo "$local_rc|$said"
    )
}
RUNNING='# 1  Short offline       Self-test routine in progress 90%      1000         -'
COMPLETED='# 1  Short offline       Completed without error       00%      1000         -'

for kind in RUNNING COMPLETED; do
    {
        echo "${!kind}"
        for ((i = 0; i < 1500; i++)); do printf '# 1 filler line %06d of the self-test log, nothing to see\n' "$i"; done
    } >"$WORK/selftest-$kind-big.txt"
    big_size_ok "$WORK/selftest-$kind-big.txt"
done
check "self-test in progress on line 1 of input over 64 KiB: still running, not ready" \
    "$(run_selftest "$WORK/selftest-RUNNING-big.txt" passthrough)" "1|running"
check "self-test completed on line 1 of input over 64 KiB: completed" \
    "$(run_selftest "$WORK/selftest-COMPLETED-big.txt" passthrough)" "0|completed"

echo "$RUNNING" >"$WORK/selftest-running.txt"
check "self-test in progress: still running, not ready" "$(run_selftest "$WORK/selftest-running.txt")" "1|running"
echo "$COMPLETED" >"$WORK/selftest-completed.txt"
check "self-test completed: completed" "$(run_selftest "$WORK/selftest-completed.txt")" "0|completed"
echo '# 1  Short offline       Aborted by host               90%      1000         -' >"$WORK/selftest-other.txt"
check "self-test neither: shown as it is" "$(run_selftest "$WORK/selftest-other.txt")" "0|other"

# ---------------------------------------------------------------------------
echo "== no producer | grep -q left in scripts/"
# ---------------------------------------------------------------------------
# Every `if`/`elif` that pipes into grep -q under these scripts' pipefail is
# the shape above. None may come back; a comment may still quote one.
left="$(awk '!/^[[:space:]]*#/ && /\|[[:space:]]*grep([[:space:]]+-[a-zA-Z]+)*[[:space:]]+-[a-zA-Z]*q/ { print FILENAME ":" FNR ": " $0 }' "$ROOT"/scripts/*.sh)"
check "no 'producer | grep -q' in scripts/" "${left:-none}" "none"

echo ""
echo "passed=$pass failed=$fail"
if ((fail == 0)); then
    echo "EARLY-EXIT READERS SUITE GREEN"
    exit 0
fi
exit 1
