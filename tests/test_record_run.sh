#!/bin/bash
# shellcheck disable=SC2034,SC2016,SC2329
# SC2034: the run state set here is read by the functions extracted from
#   backup-run.sh, so the use site is invisible to shellcheck.
# SC2016: the main() check matches the literal text "$overall_status" in the
#   script's source; expanding it would defeat it.
# SC2329: the stubs are called by that same extracted code.
#
# tests/test_record_run.sh
#
# The record step of scripts/backup-run.sh — record_run_args(),
# record_backup_run_in_db() and report_unrecorded_run() — against a stub
# btrdasd (bd DAS-Backup-Manager-6wt). Nothing here mounts or runs anything.
#
# A failed backup run used to leave no backup_runs row: an unknown snapshot
# count went out as `--snaps-created -1`, record-run refused it, and the
# refusal was logged as a warning. This suite pins the script's half:
#   - an unknown count is said with --counts-unknown, never as a number: the
#     listing failed, was never read (an abort before capture_report_data),
#     or held no field this parser knows; btrbk listing nothing is a real 0;
#   - a record that fails is a FAIL: the status line reads FAILURES DETECTED,
#     the report says the run is missing from the history, and it is sent again;
#   - a record that works changes nothing about the report.
# Whether the real binary accepts these vectors is the other half, tested in
# indexer/tests/record_run_contract.rs with the vector built by this same code.

# The shell options backup-run.sh runs under.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in decide_run_counts record_run_args record_backup_run_in_db report_unrecorded_run record_op any_op_is run_status subject_status generate_report; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done
# The script's own starting value for the counters' state: what an abort
# before capture_report_data() leaves behind.
init="$(grep -m1 '^BTRBK_LATEST_RAW_OK=' "$SCRIPT")" || { echo "FAIL: no BTRBK_LATEST_RAW_OK= line in backup-run.sh"; exit 1; }

log_info() { :; }
log_warn() { echo "WARN: $*" >>"$WORK/log"; }
log_error() { echo "ERROR: $*" >>"$WORK/log"; }
# send_report keeps what it was given: the subject status and the report.
send_report() { printf '%s\n' "$2" >>"$WORK/sent_subjects"; printf '%s\n' "$1" >"$WORK/sent_report"; }
systemctl() { :; }
generate_throughput_section() { echo "  tp"; }
generate_capacity_section() { echo "  cap"; }
generate_growth_section() { echo "  growth"; }
generate_smart_section() { echo "  smart"; }

DAS_DB_PATH="$WORK/index.db"
LAST_REPORT="$WORK/last-report.txt"
BTRBK_LATEST=""
SUBVOL_SYNC_REPORT=""
SUBVOL_EXPIRE_REPORT=""
RECOVERY_OS_REPORT=""
BTRDASD_BIN="$WORK/btrdasd"
declare -A OP_STATUS=() USAGE_BEFORE=() USAGE_AFTER=()

# Stub: keeps its arguments NUL-separated (--errors carries newlines); its
# stderr and exit code are chosen by files in $WORK.
cat >"$BTRDASD_BIN" <<'STUB'
#!/bin/bash
here="$(dirname "$0")"
printf '%s\0' "$@" >"$here/args"
echo x >>"$here/calls"
[[ -f "$here/err" ]] && cat "$here/err" >&2
exit "$(cat "$here/rc")"
STUB
chmod +x "$BTRDASD_BIN"

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }

# One source sent to two targets, a second to one; one row without a target.
raw_listing() {
    printf "%s\n" \
        "format=\"latest\" snapshot_subvolume='/v/.s/root-.1' target_subvolume='/t1/root-.1'" \
        "format=\"latest\" snapshot_subvolume='/v/.s/root-.1' target_subvolume='/t2/root-.1'" \
        "format=\"latest\" snapshot_subvolume='/v/.s/home.1' target_subvolume='/t1/home.1'" \
        "format=\"latest\" snapshot_subvolume='/v/.s/home.1' target_subvolume=''"
}

# reset: a fresh run state, counters as the script starts them.
reset() {
    OP_STATUS=([btrbk]=OK)
    USAGE_BEFORE=([/t1]=100)
    USAGE_AFTER=([/t1]=350)
    ALL_TARGET_MOUNTS=(/t1)
    BTRBK_START_TIME=1000
    BTRBK_END_TIME=1300
    BTRBK_LATEST_RAW=""
    BACKUP_RUN_RECORDED="false"
    eval "$init"
    echo 0 >"$WORK/rc"
    rm -f "$WORK/err" "$WORK/args" "$WORK/calls" "$WORK/sent_subjects" "$WORK/sent_report"
    : >"$WORK/log"
}

# The vector the last record_run_args built, one word per line ("\n" shown as |).
vector() { local a out=(); for a in "${RECORD_RUN_ARGS[@]}"; do out+=("${a//$'\n'/|}"); done; printf '%s\n' "${out[@]}"; }
# The count words: after the fixed head (backup record-run --db <path> --mode
# <m>) and before --bytes-sent.
counts_words() { vector | sed -n '7,$p' | sed '/^--bytes-sent$/,$d' | tr '\n' ' '; }

# --- the script's starting value means "not read" -------------------------------
check "the script starts the counters' state as not read" "$init" 'BTRBK_LATEST_RAW_OK=""'

# --- counts known ---------------------------------------------------------------
reset
BTRBK_LATEST_RAW_OK=true
BTRBK_LATEST_RAW="$(raw_listing)"
record_run_args SUCCESS false
# `sed -n '1,6p'` reads to the end, where `head` would quit early: no reader in
# these suites may leave its producer to die of SIGPIPE under pipefail.
check "known: head of the vector" "$(vector | sed -n '1,6p' | tr '\n' ' ')" "backup record-run --db $DAS_DB_PATH --mode incremental "
check "known: two snapshots, three sends" "$(counts_words)" "--snaps-created 2 --snaps-sent 3 "
check "known: bytes and duration" "$(vector | grep -A1 -e '^--bytes-sent$' -e '^--duration-secs$' | tr '\n' ' ')" "--bytes-sent 250 --duration-secs 300 "
check "known: success flag" "$(vector | grep -c '^--success$')" "1"
check "known: no --counts-unknown" "$(vector | grep -c '^--counts-unknown$' || true)" "0"
check "known: the counters recorded OK, with the counts" \
    "${OP_STATUS[btrbk_counters]:-unset}|${OP_STATUS[btrbk_counters_detail]:-}" "OK|2 created, 3 sent"
check "known: no errors argument" "$(vector | grep -c '^--errors$' || true)" "0"

# --- btrbk listed nothing: a real zero, not unknown ---------------------------------
reset
BTRBK_LATEST_RAW_OK=true
record_run_args SUCCESS true
check "empty listing: a measured zero" "$(counts_words)" "--snaps-created 0 --snaps-sent 0 "
check "empty listing: full mode" "$(vector | sed -n 6p)" "full"
check "empty listing: OK, a measured zero" \
    "${OP_STATUS[btrbk_counters]:-unset}|${OP_STATUS[btrbk_counters_detail]:-}" "OK|0 created, 0 sent"

# --- the listing failed: unknown (the live 2026-10-02 case) --------------------------
reset
OP_STATUS[btrbk]=FAIL
OP_STATUS[btrbk_detail]="exit code 10"
BTRBK_LATEST_RAW_OK=false
record_run_args FAILURE false
check "listing failed: --counts-unknown and no count" "$(counts_words)" "--counts-unknown "
check "listing failed: never a negative number" "$(vector | grep -c -e '^-1$' -e '^-[0-9]' || true)" "0"
check "listing failed: counter FAIL recorded" "${OP_STATUS[btrbk_counters]:-unset}" "FAIL"
check "listing failed: not marked a success" "$(vector | grep -c '^--success$' || true)" "0"
check "listing failed: both reasons in --errors" \
    "$(vector | grep -A1 '^--errors$' | tail -n1 | tr '|' '\n' | LC_ALL=C sort | tr '\n' '|')" \
    "btrbk: exit code 10|btrbk_counters: btrbk list latest failed; counts unknown|"

# --- never read: the run ended before capture_report_data ------------------------------
reset
record_run_args FAILURE false
check "never read: --counts-unknown" "$(counts_words)" "--counts-unknown "
check "never read: the reason" "${OP_STATUS[btrbk_counters_detail]:-unset}" \
    "not read — the run ended before the snapshot counters were taken"

# --- output with no field this parser knows: unknown, not 0 ------------------------------
reset
BTRBK_LATEST_RAW_OK=true
BTRBK_LATEST_RAW="format=\"latest\" snapshot_path='/v/.s/root-.1' target_path='/t1/root-.1'"
record_run_args SUCCESS false
check "unparsed: --counts-unknown" "$(counts_words)" "--counts-unknown "
check "unparsed: counter FAIL" "${OP_STATUS[btrbk_counters]:-unset}" "FAIL"
check "unparsed: warned" "$(grep -c '^WARN: .*no snapshot_subvolume/target_subvolume fields parsed' "$WORK/log")" "1"

# --- bd DAS-Backup-Manager-bzw: the counters are decided before the status ----------------
# main() decides them after capture_report_data and before run_status and the
# report, so a counter failure reads FAILURES DETECTED in the report's status
# line and is not --success in the history. It used to be recorded only inside
# record_run_args, after both were fixed: the email said ALL OPERATIONS
# SUCCESSFUL while the history listed the failure (and d1r exited 3).
reset
BTRBK_LATEST_RAW_OK=false
decide_run_counts
check "bzw: a failed listing is a FAIL before any status is read" "${OP_STATUS[btrbk_counters]:-unset}" "FAIL"
check "bzw: so the run status is FAILURE" "$(run_status)" "FAILURE"
report="$(generate_report)"
check "bzw: the report says FAILURES DETECTED" "$(grep -c '^  Status: FAILURES DETECTED$' <<<"$report")" "1"
check "bzw: and names the counters" \
    "$(grep -c '^  Snapshot counts       FAIL  (btrbk list latest failed; counts unknown)$' <<<"$report")" "1"
record_run_args "$(run_status)" false
check "bzw: the history row is not --success" "$(vector | grep -c '^--success$' || true)" "0"
check "bzw: the row carries the counts decided" "$(counts_words)" "--counts-unknown "

reset
BTRBK_LATEST_RAW_OK=true
BTRBK_LATEST_RAW="$(raw_listing)"
decide_run_counts
check "bzw: counts known: OK" "${OP_STATUS[btrbk_counters]:-unset}" "OK"
check "bzw: counts known: SUCCESS" "$(run_status)" "SUCCESS"
check "bzw: counts known: the row reads OK, with the counts" \
    "$(generate_report | grep -c '^  Snapshot counts       OK  (2 created, 3 sent)$')" "1"

# --- round 3, M6: a row nothing decided reads N/A, as every row does ---------
# decide_run_counts records OK itself, with the counts. The row's default was
# "OK (counted)", so a report built before the counts were decided would have
# read as a success — the shape of the bzw defect.
reset
BTRBK_LATEST_RAW_OK=true
BTRBK_LATEST_RAW="$(raw_listing)"
check "M6: not decided yet: the row reads N/A" \
    "$(generate_report | grep -c '^  Snapshot counts       N/A  (n/a)$')" "1"
check "M6: not decided yet: never OK" "$(generate_report | grep -c '^  Snapshot counts       OK')" "0"

# main() decides, then record_run_args decides again for its vector: the same
# answer, and the unparsed-output warning only once.
reset
BTRBK_LATEST_RAW_OK=true
BTRBK_LATEST_RAW="format=\"latest\" snapshot_path='/v/.s/root-.1' target_path='/t1/root-.1'"
decide_run_counts
record_run_args FAILURE false
check "bzw: decided twice, the same counts" "$(counts_words)" "--counts-unknown "
check "bzw: decided twice, warned once" "$(grep -c 'no snapshot_subvolume/target_subvolume fields parsed' "$WORK/log")" "1"

# The order in main(): read the counters, decide them, then the status.
check "bzw: main() decides the counts before the run status" \
    "$(extract main | grep -E '^ +(capture_report_data|decide_run_counts|overall_status="\$\(run_status\)")$' | tr -s ' ' | tr '\n' '|')" \
    ' capture_report_data| decide_run_counts| overall_status="$(run_status)"|'

# --- a record that works ----------------------------------------------------------------
reset
BTRBK_LATEST_RAW_OK=true
BTRBK_LATEST_RAW="$(raw_listing)"
record_backup_run_in_db SUCCESS false
check "recorded: the binary got the vector" "$(tr '\0' '\n' <"$WORK/args" | sed -n '1,2p' | tr '\n' ' ')" "backup record-run "
check "recorded: marked recorded" "$BACKUP_RUN_RECORDED" "true"
check "recorded: no history FAIL" "${OP_STATUS[run_history]:-unset}" "unset"
report_unrecorded_run >/dev/null
check "recorded: nothing sent again" "$([[ -f "$WORK/sent_subjects" ]] && echo sent || echo none)" "none"
report="$(generate_report)"
check "recorded: no RUN HISTORY section" "$(grep -c '^RUN HISTORY$' <<<"$report" || true)" "0"
check "recorded: status untouched" "$(grep -c '^  Status: ALL OPERATIONS SUCCESSFUL$' <<<"$report")" "1"
record_backup_run_in_db SUCCESS false
check "recorded: a second call records nothing" "$(wc -l <"$WORK/calls")" "1"

# --- a record that fails: a FAIL, and the report says the run is missing -------------------
reset
BTRBK_LATEST_RAW_OK=true
BTRBK_LATEST_RAW="$(raw_listing)"
OP_STATUS[recovery_os]=WARN
printf "error: unexpected argument '-1' found\n\nUsage: btrdasd backup record-run [OPTIONS]\n" >"$WORK/err"
echo 2 >"$WORK/rc"
check "failing record: a bare call under set -e returns and the run continues" \
    "$(set -e; record_backup_run_in_db SUCCESS false; echo continued)" "continued"
BACKUP_RUN_RECORDED="false"
: >"$WORK/log"
record_backup_run_in_db SUCCESS false
check "failing record: FAIL, not a warning" "${OP_STATUS[run_history]:-unset}" "FAIL"
check "failing record: the detail names the reason" "${OP_STATUS[run_history_detail]:-unset}" \
    "recording it failed: error: unexpected argument '-1' found"
check "failing record: logged as an error" "$(grep -c "^ERROR: This run is NOT in the backup history" "$WORK/log")" "1"
check "failing record: not logged as a mere warning" "$(grep -c '^WARN: ' "$WORK/log" || true)" "0"
check "failing record: still marked attempted, so cleanup() does not retry" "$BACKUP_RUN_RECORDED" "true"
check "failing record: the run status is FAILURE" "$(run_status)" "FAILURE"
check "failing record: FAIL outranks the WARN in the subject" "$(subject_status)" "FAILURE"
report_unrecorded_run >"$WORK/stdout"
check "failing record: the report is sent again" "$(cat "$WORK/sent_subjects" 2>/dev/null)" "FAILURE"
# Empty when nothing was sent, so a regression reports here instead of aborting.
sent="$(cat "$WORK/sent_report" 2>/dev/null)" || sent=""
check "failing record: the rebuilt report is printed for the journal too" "$(grep -c '^RUN HISTORY$' "$WORK/stdout" || true)" "1"
check "failing record: status line" "$(grep -c '^  Status: FAILURES DETECTED$' <<<"$sent")" "1"
check "failing record: RUN HISTORY section" "$(grep -c '^RUN HISTORY$' <<<"$sent")" "1"
check "failing record: the section says the run is missing" \
    "$(grep -A3 '^RUN HISTORY$' <<<"$sent" | sed -n '2,4p')" \
    "  NOT RECORDED: this run is missing from the backup history (backup_runs);
  btrdasd backup report and the GUI show the run before it as the latest.
  recording it failed: error: unexpected argument '-1' found"
check "failing record: the section comes before THROUGHPUT" \
    "$(grep -E '^(BACKUP OPERATIONS|RUN HISTORY|THROUGHPUT)$' <<<"$sent" | tr '\n' ' ')" \
    "BACKUP OPERATIONS RUN HISTORY THROUGHPUT "
check "failing record: no double blank line" "$(grep -c -Pzo '\n\n\n' <<<"$sent" || true)" "0"

# --- main() makes the call: the cases above drive report_unrecorded_run directly ---------------
check "main() sends the corrected report right after recording the run" \
    "$(extract main | grep -A1 -x '        record_backup_run_in_db "$overall_status" "$force_full"' | sed -n 2p)" \
    "        report_unrecorded_run"

# --- the binary missing entirely ---------------------------------------------------------------
reset
BTRDASD_BIN="$WORK/absent"
record_backup_run_in_db FAILURE false
check "no binary: FAIL" "${OP_STATUS[run_history]:-unset}" "FAIL"
check "no binary: the reason" "$(x="${OP_STATUS[run_history_detail]:-unset}"; echo "${x%%:*}")" "recording it failed"
BTRDASD_BIN="$WORK/btrdasd"

echo
if (( fails > 0 )); then
    echo "$fails check(s) FAILED"
    exit 1
fi
echo "RECORD RUN SUITE GREEN"
