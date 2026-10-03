#!/bin/bash
# check_recovery_os / the WARN status level / the RECOVERY OS report section
# from scripts/backup-run.sh, against a stub btrdasd (bd DAS-Backup-Manager-xd3).
# Both directions: current -> OK and ALL OPERATIONS SUCCESSFUL; stale -> WARN,
# a warnings status line, and a run still recorded as SUCCESS; a current OS
# whose boot would run btrbk (bd 1yg) -> WARN saying so, not "stale";
# unreadable or a binary without the subcommand -> FAIL with the reason, and
# the run goes on.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in check_recovery_os record_op any_op_is run_status subject_status generate_report; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done

# shellcheck disable=SC2329  # called by the eval-extracted functions
log_info() { :; }; log_warn() { echo "WARN: $*" >>"$WORK/log"; }; log_error() { echo "ERROR: $*" >>"$WORK/log"; }
declare -A OP_STATUS=()
# shellcheck disable=SC2034  # read by the functions eval-extracted above
DAS_CONFIG="$WORK/config.toml"
# shellcheck disable=SC2034
DAS_RECOVERY_OS_STATE="$WORK/recovery-os.json"
# shellcheck disable=SC2034  # read by the extracted check_recovery_os
RECOVERY_OS_TIMEOUT_SECS=300
BTRDASD_BIN="$WORK/btrdasd"

# Stub: output, stderr and exit code chosen by files in $WORK.
cat >"$BTRDASD_BIN" <<'STUB'
#!/bin/bash
here="$(dirname "$0")"
echo "$*" >>"$here/calls"
if [[ -f "$here/no_subcommand" ]]; then
    echo "error: unrecognized subcommand 'recovery-os'" >&2
    exit 2
fi
cat "$here/out"
[[ -f "$here/err" ]] && cat "$here/err" >&2
exit "$(cat "$here/rc")"
STUB
chmod +x "$BTRDASD_BIN"

# Stub for coreutils timeout: records its arguments, then either reports a
# timeout (124) or runs the command after its three option words (-k 10 N).
# shellcheck disable=SC2329  # called by the extracted check_recovery_os
timeout() {
    echo "$*" >>"$WORK/timeout_calls"
    if [[ -f "$WORK/time_out" ]]; then
        return 124
    fi
    shift 3
    "$@"
}

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }

# generate_report's helpers and host probes are stubbed; only the status line,
# the operation row and the section are under test.
hostname() { echo testhost; }
systemctl() { :; }
generate_throughput_section() { echo "  tp"; }
generate_capacity_section() { echo "  cap"; }
generate_growth_section() { echo "  growth"; }
generate_smart_section() { echo "  smart"; }
# shellcheck disable=SC2034  # read by the extracted generate_report
BTRBK_START_TIME=0
# shellcheck disable=SC2034
BTRBK_END_TIME=0
# shellcheck disable=SC2034
BTRBK_LATEST=""
# shellcheck disable=SC2034
SUBVOL_SYNC_REPORT=""
SUBVOL_EXPIRE_REPORT=""

reset() { OP_STATUS=([btrbk]=OK [subvol_sync]=OK); : >"$WORK/calls"; : >"$WORK/log"; : >"$WORK/timeout_calls"; rm -f "$WORK/err" "$WORK/no_subcommand" "$WORK/time_out"; }

# --- current ------------------------------------------------------------------
reset
printf 'RECOVERY OS\n  A  (/mnt/a/@)\n    Result              current\n' >"$WORK/out"; echo 0 >"$WORK/rc"
check_recovery_os run
check "current: OK" "${OP_STATUS[recovery_os]}" "OK"
check "current: detail" "${OP_STATUS[recovery_os_detail]}" "none stale"
check "the call is bounded by timeout -k 10 300" "$(cat "$WORK/timeout_calls")" \
    "-k 10 300 $BTRDASD_BIN recovery-os status --config $DAS_CONFIG --state-file $DAS_RECOVERY_OS_STATE"
check "current: section captured" "$(head -n1 <<<"$RECOVERY_OS_REPORT")" "RECOVERY OS"
check "real run records the state file" "$(cat "$WORK/calls")" \
    "recovery-os status --config $DAS_CONFIG --state-file $DAS_RECOVERY_OS_STATE"
check "current: run status SUCCESS" "$(run_status)" "SUCCESS"
check "current: subject SUCCESS" "$(subject_status)" "SUCCESS"
report="$(generate_report)"
check "current: ALL OPERATIONS SUCCESSFUL" "$(grep -c '^  Status: ALL OPERATIONS SUCCESSFUL$' <<<"$report")" "1"
check "current: operation row" "$(grep -c '^  Recovery OS           OK  (' <<<"$report")" "1"
check "current: section in the report" "$(grep -c '^RECOVERY OS$' <<<"$report")" "1"
check "no double blank line" "$(grep -c -Pzo '\n\n\n' <<<"$report" || true)" "0"

# The section sits after RETIRED SUBVOLUMES and before THROUGHPUT.
SUBVOL_EXPIRE_REPORT=$'RETIRED SUBVOLUMES\n  @opt'
report="$(generate_report)"
check "section order" "$(grep -E '^(RETIRED SUBVOLUMES|RECOVERY OS|THROUGHPUT)$' <<<"$report" | tr '\n' ' ')" \
    "RETIRED SUBVOLUMES RECOVERY OS THROUGHPUT "
check "still no double blank line" "$(grep -c -Pzo '\n\n\n' <<<"$report" || true)" "0"
# shellcheck disable=SC2034  # read by the extracted generate_report
SUBVOL_EXPIRE_REPORT=""

# --- dry run: read, never record ------------------------------------------------
reset
check_recovery_os dryrun
check "dry run: no --state-file" "$(cat "$WORK/calls")" "recovery-os status --config $DAS_CONFIG"
check "dry run: still recorded OK" "${OP_STATUS[recovery_os]}" "OK"

# --- stale ------------------------------------------------------------------
reset
printf 'RECOVERY OS\n  A  (/mnt/a/@)\n    Result              STALE — last upgrade unknown\n' >"$WORK/out"; echo 1 >"$WORK/rc"
rc=0; check_recovery_os run || rc=$?
check "stale: returns 0" "$rc" "0"
check "stale: WARN, not FAIL" "${OP_STATUS[recovery_os]}" "WARN"
check "stale: detail" "${OP_STATUS[recovery_os_detail]}" "stale — see RECOVERY OS in the report"
check "stale: logged as a warning" "$(grep -c '^WARN: .*recovery OS' "$WORK/log")" "1"
report="$(generate_report)"
check "stale: status says warnings" "$(grep -c '^  Status: COMPLETED WITH WARNINGS$' <<<"$report")" "1"
check "stale: not ALL OPERATIONS SUCCESSFUL" "$(grep -c 'ALL OPERATIONS SUCCESSFUL' <<<"$report" || true)" "0"
check "stale: not FAILURES DETECTED" "$(grep -c 'FAILURES DETECTED' <<<"$report" || true)" "0"
check "stale: row shows WARN" "$(grep -c '^  Recovery OS           WARN  (stale' <<<"$report")" "1"
check "stale: the run is still recorded as SUCCESS" "$(run_status)" "SUCCESS"
check "stale: the subject says so" "$(subject_status)" "SUCCESS WITH WARNINGS"
# A failure elsewhere outranks a warning.
OP_STATUS[btrbk]=FAIL
check "FAIL outranks WARN: run" "$(run_status)" "FAILURE"
check "FAIL outranks WARN: subject" "$(subject_status)" "FAILURE"
check "FAIL outranks WARN: status line" "$(generate_report | grep -c '^  Status: FAILURES DETECTED$')" "1"

# --- current, but btrbk would run when it boots (bd 1yg) -------------------------
# Exit 1 with a current Result: the WARNING row is why, and the detail says so.
boot_warning='btrbk will run when this OS boots (btrbk.timer enabled, /etc/btrbk/btrbk.conf present): check its config before booting it, on bare metal or in the update VM'
reset
printf 'RECOVERY OS\n  A  (/mnt/a/@)\n    Enabled timers      btrbk.timer\n    btrbk config        /etc/btrbk/btrbk.conf (412 bytes)\n    WARNING             %s\n    Result              current\n' \
    "$boot_warning" >"$WORK/out"; echo 1 >"$WORK/rc"
rc=0; check_recovery_os run || rc=$?
check "boot warning: returns 0" "$rc" "0"
check "boot warning: WARN, not FAIL" "${OP_STATUS[recovery_os]}" "WARN"
check "boot warning: detail says btrbk, not stale" "${OP_STATUS[recovery_os_detail]}" "btrbk may run at boot — see RECOVERY OS in the report"
check "boot warning: logged once, as a warning" "$(grep -c '^WARN: ' "$WORK/log")" "1"
check "boot warning: the log line names btrbk" "$(grep -c '^WARN: btrbk may run when a recovery OS boots' "$WORK/log")" "1"
report="$(generate_report)"
check "boot warning: status says warnings" "$(grep -c '^  Status: COMPLETED WITH WARNINGS$' <<<"$report")" "1"
check "boot warning: row shows WARN" "$(grep -c '^  Recovery OS           WARN  (btrbk may run at boot' <<<"$report")" "1"
check "boot warning: the WARNING row reaches the report" "$(grep -c "^    WARNING             $boot_warning\$" <<<"$report")" "1"
check "boot warning: the run is still recorded as SUCCESS" "$(run_status)" "SUCCESS"
check "boot warning: the subject says so" "$(subject_status)" "SUCCESS WITH WARNINGS"
check "boot warning: the drive counts as inspected, not unmounted" "$(grep -c 'not mounted' <<<"${OP_STATUS[recovery_os_detail]}" || true)" "0"

# Stale and warned at once: both said, in that order, one log line each.
reset
printf 'RECOVERY OS\n  A  (/mnt/a/@)\n    WARNING             %s\n    Result              STALE — x\n  B  not mounted\n' \
    "$boot_warning" >"$WORK/out"; echo 1 >"$WORK/rc"
check_recovery_os run
check "stale and warned: both in the detail" "${OP_STATUS[recovery_os_detail]}" \
    "stale; btrbk may run at boot — see RECOVERY OS in the report; not mounted: B"
check "stale and warned: two warning lines logged" "$(grep -c '^WARN: ' "$WORK/log")" "2"

# Exit 1 with neither row (a binary that says something else): still WARN,
# without claiming a reason the section does not show.
reset
printf 'RECOVERY OS\n  A  (/mnt/a/@)\n    Result              current\n' >"$WORK/out"; echo 1 >"$WORK/rc"
check_recovery_os run
check "exit 1, no row: WARN" "${OP_STATUS[recovery_os]}" "WARN"
check "exit 1, no row: a neutral detail" "${OP_STATUS[recovery_os_detail]}" "needs attention — see RECOVERY OS in the report"
check "exit 1, no row: logged once" "$(grep -c '^WARN: ' "$WORK/log")" "1"

# A WARNING row with exit 0 is not possible from btrdasd; were it to appear,
# the exit status still decides (OK), as for every other row.
reset
printf 'RECOVERY OS\n  A  (/mnt/a/@)\n    WARNING             x\n    Result              current\n' >"$WORK/out"; echo 0 >"$WORK/rc"
check_recovery_os run
check "exit 0 decides: OK" "${OP_STATUS[recovery_os]}" "OK"

# --- unreadable root / state not recorded -------------------------------------
reset
printf 'RECOVERY OS\n  A  (/mnt/a/@)  UNREADABLE: /mnt/a/@ is not a directory\n' >"$WORK/out"
printf 'Error: could not record the result: /var/lib/x: Permission denied\n' >"$WORK/err"
echo 2 >"$WORK/rc"
rc=0; check_recovery_os run || rc=$?
check "unreadable: returns 0, the run goes on" "$rc" "0"
check "unreadable: FAIL" "${OP_STATUS[recovery_os]}" "FAIL"
check "unreadable: detail carries the reason" "${OP_STATUS[recovery_os_detail]}" \
    "exit code 2: Error: could not record the result: /var/lib/x: Permission denied"
check "unreadable: section kept" "$(sed -n 2p <<<"$RECOVERY_OS_REPORT")" "  A  (/mnt/a/@)  UNREADABLE: /mnt/a/@ is not a directory"
check "unreadable: run FAILURE" "$(run_status)" "FAILURE"

# --- a btrdasd without the subcommand -----------------------------------------
reset
touch "$WORK/no_subcommand"
check "old binary: a bare call under set -e returns and the run continues" \
    "$(set -e; check_recovery_os run; echo continued)" "continued"
: >"$WORK/log"
check_recovery_os run
check "old binary: FAIL" "${OP_STATUS[recovery_os]}" "FAIL"
check "old binary: detail" "${OP_STATUS[recovery_os_detail]}" \
    "exit code 2: error: unrecognized subcommand 'recovery-os'"
check "old binary: the section says the check failed" "$RECOVERY_OS_REPORT" \
    $'RECOVERY OS\n  CHECK FAILED: exit code 2: error: unrecognized subcommand \'recovery-os\''
check "old binary: error logged" "$(grep -c '^ERROR: .*recovery OS' "$WORK/log")" "1"

# --- the binary missing entirely ----------------------------------------------
reset
BTRDASD_BIN="$WORK/absent"
check_recovery_os run
check "no binary: FAIL" "${OP_STATUS[recovery_os]}" "FAIL"
check "no binary: detail names the exit code" "${OP_STATUS[recovery_os_detail]%%:*}" "exit code 127"
BTRDASD_BIN="$WORK/btrdasd"

# --- not mounted drives are named in the detail --------------------------------
reset
printf 'RECOVERY OS\n  A  (/mnt/a/@)\n    Result              current\n  B  not mounted\n  C D  not mounted\n' >"$WORK/out"; echo 0 >"$WORK/rc"
check_recovery_os run
check "some unmounted: detail names them" "${OP_STATUS[recovery_os_detail]}" "none stale; not mounted: B, C D"
printf 'RECOVERY OS\n  B  not mounted\n' >"$WORK/out"
check_recovery_os run
check "none mounted: nothing inspected, and which" "${OP_STATUS[recovery_os_detail]}" "nothing inspected; not mounted: B"
printf 'RECOVERY OS\n  A  (/mnt/a/@)\n    Result              STALE — x\n  B  not mounted\n' >"$WORK/out"; echo 1 >"$WORK/rc"
check_recovery_os run
check "stale and unmounted: both said" "${OP_STATUS[recovery_os_detail]}" "stale — see RECOVERY OS in the report; not mounted: B"

# --- a stalled read times out ----------------------------------------------------
reset
touch "$WORK/time_out"
check "timeout: a bare call under set -e returns and the run continues" \
    "$(set -e; check_recovery_os run; echo continued)" "continued"
: >"$WORK/log"
check_recovery_os run
check "timeout: FAIL" "${OP_STATUS[recovery_os]}" "FAIL"
check "timeout: detail" "${OP_STATUS[recovery_os_detail]}" "recovery-os status timed out after 300 s (drive stalled?)"
check "timeout: the section says so" "$RECOVERY_OS_REPORT" \
    $'RECOVERY OS\n  CHECK FAILED: recovery-os status timed out after 300 s (drive stalled?)'
check "timeout: the run status is FAILURE, the exit rule is untouched" "$(run_status)" "FAILURE"
check "timeout: logged as an error" "$(grep -c '^ERROR: .*timed out after 300 s' "$WORK/log")" "1"

# --- a corrupt state file: the way out reaches the report ----------------------
reset
printf 'RECOVERY OS\n  A  (/mnt/a/@)\n    Result              current\n' >"$WORK/out"
printf "Error: could not record the result: %s: expected value — left as it is; remove it to start over: rm -- '%s'\n" \
    "$DAS_RECOVERY_OS_STATE" "$DAS_RECOVERY_OS_STATE" >"$WORK/err"
echo 2 >"$WORK/rc"
check_recovery_os run
check "corrupt state: FAIL with the fix in the detail" "${OP_STATUS[recovery_os_detail]}" \
    "exit code 2: Error: could not record the result: $DAS_RECOVERY_OS_STATE: expected value — left as it is; remove it to start over: rm -- '$DAS_RECOVERY_OS_STATE'"

# --- one name for the state path --------------------------------------------------
# shellcheck disable=SC2016
check "the script defaults DAS_RECOVERY_OS_STATE, the name the Rust side reads" \
    "$(grep -c '^DAS_RECOVERY_OS_STATE="${DAS_RECOVERY_OS_STATE:-/var/lib/das-backup/recovery-os.json}"$' "$SCRIPT")" "1"
check "no other name for it remains" "$(grep -cE '(\$\{?|^)RECOVERY_OS_STATE([^_A-Z]|$)' "$SCRIPT" || true)" "0"
# shellcheck disable=SC2016
check "the timeout bound is 300 s" "$(grep -c '^RECOVERY_OS_TIMEOUT_SECS=300$' "$SCRIPT")" "1"

# --- main(): wiring -------------------------------------------------------------
main_body="$(awk '/^main\(\) \{/,/^}/' "$SCRIPT")"
# The patterns below are literal script text, not expansions.
# shellcheck disable=SC2016
# shellcheck disable=SC2016
check "main: the check runs after btrbk and expiry, before unmount" \
    "$(grep -n -E '^ +(run_btrbk "\$mode"|expire_retired_subvolumes "\$mode"|check_recovery_os "\$mode"|unmount_all)$' <<<"$main_body" | cut -d: -f2 | tr -s ' ' | tr '\n' '|')" \
    ' run_btrbk "$mode"| expire_retired_subvolumes "$mode"| check_recovery_os "$mode"| unmount_all| unmount_all|'
# shellcheck disable=SC2016
check "main: the DB status comes from run_status" "$(grep -c 'overall_status="\$(run_status)"' <<<"$main_body")" "1"
# shellcheck disable=SC2016
check "main: the subject comes from subject_status" "$(grep -c 'send_report "\$report" "\$(subject_status)"' <<<"$main_body")" "1"

if [[ $fails -eq 0 ]]; then
    echo "ALL RECOVERY OS SHELL TESTS PASSED"
else
    echo "$fails FAILED"
    exit 1
fi
