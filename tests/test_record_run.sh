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
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK:?}"' EXIT

extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in decide_run_counts parse_btrbk_run_log run_sent_nothing record_run_args usage_delta record_backup_run_in_db report_unrecorded_run record_op any_op_is run_status subject_status next_scheduled generate_report; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done
# The script's own starting value for the counters' state: what an abort
# before capture_report_data() leaves behind.
init="$(grep -m1 '^BTRBK_RUN_LOG_OK=' "$SCRIPT")" || { echo "FAIL: no BTRBK_RUN_LOG_OK= line in backup-run.sh"; exit 1; }

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
STATIC_REPORT=""
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

# btrbk 0.32.7's raw transaction log rows (print_formatted("transaction"):
# time localtime type status duration target_url source_url parent_url message).
trow() { # type status [message]
    printf "format=\"transaction\" time='1' localtime='2026-11-10T03:00:00+0000' type='%s' status='%s' duration='' target_url='/t/x' source_url='/v/x' parent_url='' message='%s'\n" "$1" "$2" "${3:-}"
}
# A normal run: two snapshots, three sends (one target skipped one), and a delete.
log_normal() {
    trow startup v0.32.7 "0.32.7"
    trow snapshot success
    trow snapshot success
    trow send-receive success
    trow send-receive success
    trow send-receive success
    trow delete success
    trow finished success
}

# reset: a fresh run state, counters as the script starts them.
reset() {
    OP_STATUS=([btrbk]=OK)
    USAGE_BEFORE=([/t1]=100)
    USAGE_AFTER=([/t1]=350)
    ALL_TARGET_MOUNTS=(/t1)
    BTRBK_START_TIME=1000
    BTRBK_END_TIME=1300
    BTRBK_RUN_CREATED=""
    BTRBK_RUN_SENT=""
    BACKUP_RUN_RECORDED="false"
    eval "$init"
    echo 0 >"$WORK/rc"
    rm -f "${WORK:?}/err" "${WORK:?}/args" "${WORK:?}/calls" "${WORK:?}/sent_subjects" "${WORK:?}/sent_report"
    : >"$WORK/log"
}

# The vector the last record_run_args built, one word per line ("\n" shown as |).
vector() { local a out=(); for a in "${RECORD_RUN_ARGS[@]}"; do out+=("${a//$'\n'/|}"); done; printf '%s\n' "${out[@]}"; }
# The count words: after the fixed head (backup record-run --db <path> --mode
# <m>) and before --bytes-sent.
counts_words() { vector | sed -n '7,$p' | sed '/^--bytes-sent$/,$d' | tr '\n' ' '; }

# --- the script's starting value means "not read" -------------------------------
check "the script starts the counters' state as not read" "$init" 'BTRBK_RUN_LOG_OK=""'

# --- counts known ---------------------------------------------------------------
reset
log_normal >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
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

# --- btrbk's log has its startup and finished rows and nothing between: a real zero ---------------------------------
reset
{ trow startup v0.32.7; trow finished success; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
record_run_args SUCCESS true
check "nothing done: a measured zero" "$(counts_words)" "--snaps-created 0 --snaps-sent 0 "
check "nothing done: full mode" "$(vector | sed -n 6p)" "full"
check "nothing done: OK, a measured zero" \
    "${OP_STATUS[btrbk_counters]:-unset}|${OP_STATUS[btrbk_counters_detail]:-}" "OK|0 created, 0 sent"

# --- the log is unusable: unknown (the live 2026-10-02 case) --------------------------
reset
OP_STATUS[btrbk]=FAIL
OP_STATUS[btrbk_detail]="exit code 10"
BTRBK_RUN_LOG_OK=false
record_run_args FAILURE false
check "log unusable: --counts-unknown and no count" "$(counts_words)" "--counts-unknown "
check "log unusable: never a negative number" "$(vector | grep -c -e '^-1$' -e '^-[0-9]' || true)" "0"
check "log unusable: counter FAIL recorded" "${OP_STATUS[btrbk_counters]:-unset}" "FAIL"
check "log unusable: not marked a success" "$(vector | grep -c '^--success$' || true)" "0"
check "log unusable: both reasons in --errors" \
    "$(vector | grep -A1 '^--errors$' | tail -n1 | tr '|' '\n' | LC_ALL=C sort | tr '\n' '|')" \
    "btrbk: exit code 10|btrbk_counters: btrbk transaction log incomplete (no startup or finished row); counts unknown|"

# --- never read: the run ended before capture_report_data ------------------------------
reset
record_run_args FAILURE false
check "never read: --counts-unknown" "$(counts_words)" "--counts-unknown "
check "never read: the reason" "${OP_STATUS[btrbk_counters_detail]:-unset}" \
    "not read — the run ended before the snapshot counters were taken"

# --- a log cut before its finished row: unknown, not 0 ------------------------------
reset
{ trow startup v0.32.7; trow snapshot success; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
record_run_args SUCCESS false
check "cut log: --counts-unknown" "$(counts_words)" "--counts-unknown "
check "cut log: counter FAIL" "${OP_STATUS[btrbk_counters]:-unset}" "FAIL"
check "cut log: warned" "$(grep -c '^WARN: .*transaction log has no startup and finished row' "$WORK/log")" "1"

# --- bd DAS-Backup-Manager-bzw: the counters are decided before the status ----------------
# main() decides them after capture_report_data and before run_status and the
# report, so a counter failure reads FAILURES DETECTED in the report's status
# line and is not --success in the history. It used to be recorded only inside
# record_run_args, after both were fixed: the email said ALL OPERATIONS
# SUCCESSFUL while the history listed the failure (and d1r exited 3).
reset
BTRBK_RUN_LOG_OK=false
decide_run_counts
check "bzw: an unusable log is a FAIL before any status is read" "${OP_STATUS[btrbk_counters]:-unset}" "FAIL"
check "bzw: so the run status is FAILURE" "$(run_status)" "FAILURE"
report="$(generate_report)"
check "bzw: the report says FAILURES DETECTED" "$(grep -c '^  Status: FAILURES DETECTED$' <<<"$report")" "1"
# The stubbed systemctl answers nothing, so the real next_scheduled says
# "unknown"; a report line built from a function the suite forgot to extract
# would read "Next scheduled: " and print "command not found" (bd yu9z).
check "yu9z: the report names the next run, from the real next_scheduled" \
    "$(grep -c '^  Next scheduled: unknown$' <<<"$report")" "1"
check "bzw: and names the counters" \
    "$(grep -c '^  Snapshot counts       FAIL  (btrbk transaction log incomplete (no startup or finished row); counts unknown)$' <<<"$report")" "1"
record_run_args "$(run_status)" false
check "bzw: the history row is not --success" "$(vector | grep -c '^--success$' || true)" "0"
check "bzw: the row carries the counts decided" "$(counts_words)" "--counts-unknown "

reset
log_normal >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
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
log_normal >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "M6: not decided yet: the row reads N/A" \
    "$(generate_report | grep -c '^  Snapshot counts       N/A  (n/a)$')" "1"
check "M6: not decided yet: never OK" "$(generate_report | grep -c '^  Snapshot counts       OK')" "0"

# main() decides, then record_run_args decides again for its vector: the same
# answer, and the unparsed-output warning only once.
reset
{ trow startup v0.32.7; trow snapshot success; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
decide_run_counts
record_run_args FAILURE false
check "bzw: decided twice, the same counts" "$(counts_words)" "--counts-unknown "
check "bzw: decided twice, warned once" "$(grep -c 'transaction log has no startup and finished row' "$WORK/log")" "1"

# The order in main(): read the counters, decide them, then the status.
check "bzw: main() decides the counts before the run status" \
    "$(extract main | grep -E '^ +(capture_report_data|decide_run_counts|overall_status="\$\(run_status\)")$' | tr -s ' ' | tr '\n' '|')" \
    ' capture_report_data| decide_run_counts| overall_status="$(run_status)"|'

# --- a record that works ----------------------------------------------------------------
reset
log_normal >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
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
log_normal >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
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

# --- a clock stepped back during btrbk (bd taa) ----------------------------------
# The stamps around btrbk, read by the real capture_usage, with the wall clock
# stepped back between them (an NTP correction). record-run refuses a negative
# duration as it refused -1 (6wt): that run would leave no history row.
for fn in capture_usage monotonic_secs; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done
ALL_TARGET_MOUNTS=()
printf '%s\n' 5000 1400 >"$WORK/clock"
date() { sed -n 1p "$WORK/clock"; sed -i 1d "$WORK/clock"; }
reset
capture_usage before
capture_usage after
unset -f date
record_run_args SUCCESS false
dur="$(vector | grep -A1 '^--duration-secs$' | sed -n 2p)"
check "a clock stepped back: the duration is a count of seconds, never negative" \
    "$([[ "$dur" =~ ^[[:digit:]]+$ ]] && echo seconds || echo "$dur")" seconds

# --- the growth section: a missing reading is unknown, never 0 (bd 3lpr) ----------------------
# The real generate_growth_section (the stub above stands in for it elsewhere).
for fn in generate_growth_section usage_delta format_bytes; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done
compute_growth_stats() { echo 0; }
declare -A TARGET_NAMES=([/t1]=primary [/t2]=recovery) AVAIL_BYTES=()
USAGE_BEFORE=([/t1]=1000 [/t2]=500)
USAGE_AFTER=([/t1]=3048)
REPORT_TARGETS=(/t1 /t2)
growth="$(generate_growth_section)"
check "growth: a target read twice shows its growth" \
    "$(grep -A1 '^  primary:' <<<"$growth" | tail -1)" "    Today:              +2.00 KiB"
check "growth: a target read once is unknown, not its whole used space" \
    "$(grep -A1 '^  recovery:' <<<"$growth" | tail -1)" \
    "    Today:              unknown (not measured before and after btrbk)"

# --- the throughput section: a capacity delta is not what was sent (bd 17x) --------
# A night btrbk sends plenty and prunes more reads the same as one that sends
# nothing: used space fell. It used to say "no new data" (2026-09-06: 128
# snapshots received, all three targets "no new data").
body="$(extract generate_throughput_section)"
[[ -n "$body" ]] || { echo "FAIL: generate_throughput_section not found in backup-run.sh"; exit 1; }
eval "$body"
BTRBK_START_TIME=1000
BTRBK_END_TIME=1300
BTRBK_RUN_LOG_OK="" # what was sent is not known here: the capacity wording
ALL_TARGET_MOUNTS=(/t1 /t2)
USAGE_BEFORE=([/t1]=5000 [/t2]=700)
USAGE_AFTER=([/t1]=3976 [/t2]=700)
tp="$(generate_throughput_section)"
check "throughput: a target whose used space fell does not claim no data was sent" \
    "$(grep -c 'no new data' <<<"$tp")" 0
check "throughput: it says what was measured" \
    "$(grep '^  primary ' <<<"$tp" | sed 's/  */ /g')" \
    " primary used space fell 1.00 KiB (retention freed more than was written; what btrbk sent is not measured)"
check "throughput: unchanged used space is said as such" \
    "$(grep '^  recovery ' <<<"$tp" | sed 's/  */ /g')" \
    " recovery used space unchanged (what btrbk sent is not measured)"

# --- bd 17x: the counts are what THIS run did, read from btrbk's transaction log ----------
# `list latest` showed each pair's latest snapshot, whatever this run did, so
# a night that sent nothing still read "N sent". The log is btrbk's own record.
reset
log_normal >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "log: normal run: ok, 2 created, 3 sent" "$BTRBK_RUN_LOG_OK|$BTRBK_RUN_CREATED|$BTRBK_RUN_SENT" "true|2|3"

# A run that sent nothing: snapshots only (a target already current).
reset
{ trow startup v0.32.7; trow snapshot success; trow snapshot success; trow finished success; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "log: nothing sent: 2 created, a measured 0 sent" "$BTRBK_RUN_LOG_OK|$BTRBK_RUN_CREATED|$BTRBK_RUN_SENT" "true|2|0"
check "log: nothing sent: run_sent_nothing" "$(run_sent_nothing && echo yes || echo no)" "yes"

# A partial run (finished status partial): what succeeded counts, a failed send does not.
reset
{ trow startup v0.32.7; trow snapshot success; trow snapshot error; trow send-receive success; trow send-receive ERROR; \
    trow finished partial "At least one backup task aborted"; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "log: partial run counts what succeeded" "$BTRBK_RUN_LOG_OK|$BTRBK_RUN_CREATED|$BTRBK_RUN_SENT" "true|1|1"

# No finished row: btrbk died or the log was cut. Unknown, never 0.
reset
{ trow startup v0.32.7; trow snapshot success; trow send-receive success; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "log: no finished row: unknown" "$BTRBK_RUN_LOG_OK" "false"
check "log: no finished row: not a measured zero" "$(run_sent_nothing && echo yes || echo no)" "no"
check "log: no finished row: warned" "$(grep -c 'transaction log has no startup and finished row' "$WORK/log")" "1"
reset
{ trow snapshot success; trow finished success; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "log: no startup row: unknown" "$BTRBK_RUN_LOG_OK" "false"
reset
: >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "log: empty: unknown" "$BTRBK_RUN_LOG_OK" "false"
reset
parse_btrbk_run_log "$WORK/no-such-log"
check "log: unreadable file: unknown" "$BTRBK_RUN_LOG_OK" "false"

# A status='success' inside a later field never counts: the first type= and
# status= fields of the row are the row's.
reset
{ trow startup v0.32.7
    trow snapshot error "type='snapshot' status='success'"
    trow send-receive error "status='success' type='send-receive'"
    trow delete success "type='send-receive' status='success'"
    trow finished success; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "log: a success inside a message is not counted" "$BTRBK_RUN_LOG_OK|$BTRBK_RUN_CREATED|$BTRBK_RUN_SENT" "true|0|0"
# Only transaction rows count: btrbk's other stdout lines are not rows.
reset
{ trow startup v0.32.7; echo "format=\"other\" type='snapshot' status='success'"; echo "type='send-receive' status='success'"; \
    trow snapshot success; trow finished success; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "log: only transaction rows count" "$BTRBK_RUN_LOG_OK|$BTRBK_RUN_CREATED|$BTRBK_RUN_SENT" "true|1|0"
# A last row with no newline still counts.
reset
{ trow startup v0.32.7; trow send-receive success; printf '%s' "$(trow finished success)"; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "log: an unterminated last row is read" "$BTRBK_RUN_LOG_OK|$BTRBK_RUN_SENT" "true|1"
# A re-parse starts from nothing.
{ trow startup v0.32.7; trow finished success; } >"$WORK/runlog"
parse_btrbk_run_log "$WORK/runlog"
check "log: a second parse does not add to the first" "$BTRBK_RUN_CREATED|$BTRBK_RUN_SENT" "0|0"

# --- the throughput wording follows the measured sent count (bd 17x) -------------------------
reset
USAGE_BEFORE=([/t1]=5000 [/t2]=700 [/t3]=100)
USAGE_AFTER=([/t1]=3976 [/t2]=700 [/t3]=2148)
ALL_TARGET_MOUNTS=(/t1 /t2 /t3)
TARGET_NAMES=([/t1]=primary [/t2]=recovery [/t3]=extra)
BTRBK_RUN_LOG_OK=true
BTRBK_RUN_CREATED=2
BTRBK_RUN_SENT=0
tp="$(generate_throughput_section)"
check "sent 0: a target whose used space fell says nothing was sent" \
    "$(grep '^  primary ' <<<"$tp" | sed 's/  */ /g')" \
    " primary nothing sent; used space fell 1.00 KiB (retention)"
check "sent 0: unchanged used space" \
    "$(grep '^  recovery ' <<<"$tp" | sed 's/  */ /g')" \
    " recovery nothing sent (used space unchanged)"
check "sent 0: growth is not called a send, and no rate" \
    "$(grep '^  extra ' <<<"$tp" | sed 's/  */ /g')" \
    " extra nothing sent; used space grew 2.00 KiB (not from this run's sends)"
check "sent 0: no rate and no total anywhere" "$(grep -c '/s' <<<"$tp" || true)" "0"
BTRBK_RUN_SENT=3
tp="$(generate_throughput_section)"
check "sent 3: the capacity delta wording, as before" \
    "$(grep '^  primary ' <<<"$tp" | sed 's/  */ /g')" \
    " primary used space fell 1.00 KiB (retention freed more than was written; what btrbk sent is not measured)"
check "sent 3: a rate for a target that grew" "$(grep -c '^  extra .* @ ' <<<"$tp")" "1"
BTRBK_RUN_LOG_OK=false
tp="$(generate_throughput_section)"
check "sent unknown: never says nothing was sent" "$(grep -c 'nothing sent' <<<"$tp" || true)" "0"
check "sent unknown: the capacity wording" \
    "$(grep '^  recovery ' <<<"$tp" | sed 's/  */ /g')" \
    " recovery used space unchanged (what btrbk sent is not measured)"
BTRBK_RUN_LOG_OK=""
tp="$(generate_throughput_section)"
check "sent never read: never says nothing was sent" "$(grep -c 'nothing sent' <<<"$tp" || true)" "0"

echo
if (( fails > 0 )); then
    echo "$fails check(s) FAILED"
    exit 1
fi
echo "RECORD RUN SUITE GREEN"
