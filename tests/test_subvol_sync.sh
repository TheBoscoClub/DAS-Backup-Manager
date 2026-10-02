#!/bin/bash
# sync_subvolumes / expire_retired_subvolumes / load_config_env from
# scripts/backup-run.sh, exercised against a stub btrdasd. Both directions:
# a clean sync records OK and reloads the config; a failing one records FAIL,
# keeps the run going, and still reloads.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in load_config_env sync_subvolumes expire_retired_subvolumes record_op generate_report; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done

# shellcheck disable=SC2329  # called by the eval-extracted functions
log_info() { :; }; log_warn() { echo "WARN: $*" >>"$WORK/log"; }; log_error() { echo "ERROR: $*" >>"$WORK/log"; }
declare -A OP_STATUS=()
# shellcheck disable=SC2034  # read by the functions eval-extracted above
DAS_CONFIG="$WORK/config.toml"
DAS_DB_PATH="$WORK/index.db"
BTRDASD_BIN="$WORK/btrdasd"

# Stub: behaviour chosen by files in $WORK.
cat >"$BTRDASD_BIN" <<'STUB'
#!/bin/bash
here="$(dirname "$0")"
echo "$*" >>"$here/calls"
case "$1 $2" in
    "config dump-env")
        if [[ -f "$here/env_fail" ]]; then
            echo "stub: cannot read config" >&2
            exit 1
        fi
        n=$(cat "$here/source_count")
        echo "DAS_SOURCE_COUNT=$n"
        for ((i = 0; i < n; i++)); do
            echo "DAS_SOURCE_${i}_LABEL='src$i'"
            echo "DAS_SOURCE_${i}_VOLUME='/vol$i'"
            echo "DAS_SOURCE_${i}_DEVICE='UUID=u$i'"
            echo "DAS_SOURCE_${i}_SNAPSHOT_DIR='.btrbk-snapshots'"
        done
        # Targets deliberately carry NO DISPLAY_NAME: dump-env omits it when empty.
        t=0; [[ -f "$here/target_count" ]] && t=$(cat "$here/target_count")
        echo "DAS_TARGET_COUNT=$t"
        mounts=""
        for ((i = 0; i < t; i++)); do
            echo "DAS_TARGET_${i}_LABEL='tgt$i'"
            echo "DAS_TARGET_${i}_SERIAL='ser$i'"
            echo "DAS_TARGET_${i}_SERIALS='ser$i'"
            echo "DAS_TARGET_${i}_MOUNT_UUID=''"
            echo "DAS_TARGET_${i}_MOUNT='/mnt/t$i'"
            echo "DAS_TARGET_${i}_ROLE='primary'"
            mounts="$mounts /mnt/t$i"
        done
        echo "DAS_ALL_TARGET_MOUNTS='${mounts# }'"
        echo "DAS_BTRBK_CONF='/etc/btrbk/btrbk.conf'"
        ;;
    "subvol sync")   cat "$here/sync_out";   exit "$(cat "$here/sync_rc")" ;;
    "subvol expire") cat "$here/expire_out"; exit "$(cat "$here/expire_rc")" ;;
esac
STUB
chmod +x "$BTRDASD_BIN"

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }

# --- load_config_env rebuilds the per-source arrays from scratch ------------
echo 2 >"$WORK/source_count"
load_config_env
check "two sources loaded" "${#SOURCE_VOLUMES[@]}" "2"
echo 3 >"$WORK/source_count"
load_config_env
check "third source appears after reload" "${SOURCE_VOLUMES[src2]:-}" "/vol2"
echo 1 >"$WORK/source_count"
load_config_env
check "removed sources do not linger" "${#SOURCE_VOLUMES[@]}" "1"

# --- load_config_env: targets, with no DISPLAY_NAME emitted -----------------
echo 2 >"$WORK/target_count"
rc=0; load_config_env || rc=$?
check "targets without a display name load" "$rc" "0"
check "target arrays filled" "${#TARGET_MOUNTS[@]}/${#ALL_TARGET_MOUNTS[@]}" "2/2"
check "missing display name falls back to the label" "${TARGET_NAMES[/mnt/t1]:-}" "tgt1"
echo 1 >"$WORK/target_count"
load_config_env
check "removed targets do not linger" "${#TARGET_MOUNTS[@]}/${#TARGET_NAMES[@]}/${#MOUNT_ROLES[@]}/${#ALL_TARGET_MOUNTS[@]}" "1/1/1/1"
echo 0 >"$WORK/target_count"

# --- load_config_env: unreadable config -------------------------------------
touch "$WORK/env_fail"
rc=0; load_config_env 2>"$WORK/err" || rc=$?
check "unreadable config makes load_config_env return non-zero" "$rc" "1"
check "unreadable config leaves an error on stderr" "$(grep -c 'could not read' "$WORK/err")" "1"
rm -f "$WORK/env_fail"

# verify_sources_before_write is the real one's stand-in here: count calls.
verify_calls=0
verify_sources_before_write() { verify_calls=$((verify_calls + 1)); }

# --- sync: clean ------------------------------------------------------------
: >"$WORK/calls"
printf 'SUBVOLUME SYNC\n  Adopted (now backed up):\n    @new\n' >"$WORK/sync_out"; echo 0 >"$WORK/sync_rc"
echo 2 >"$WORK/source_count"
sync_subvolumes run
check "clean sync records OK" "${OP_STATUS[subvol_sync]}" "OK"
check "report captured" "$(head -n1 <<<"$SUBVOL_SYNC_REPORT")" "SUBVOLUME SYNC"
check "config reloaded after sync" "${#SOURCE_VOLUMES[@]}" "2"
check "real run passes no --dry-run" "$(grep -c -- '--dry-run' "$WORK/calls" || true)" "0"
check "successful reload re-verifies the sources once" "$verify_calls" "1"

# --- sync: failure does not stop the run -----------------------------------
printf 'SUBVOLUME SYNC\n  VOLUMES NOT READ\n' >"$WORK/sync_out"; echo 1 >"$WORK/sync_rc"
rc=0; sync_subvolumes run || rc=$?
check "failing sync returns 0 so the backup continues" "$rc" "0"
check "failing sync records FAIL" "${OP_STATUS[subvol_sync]}" "FAIL"
check "failure detail names the exit code" "${OP_STATUS[subvol_sync_detail]}" "exit code 1 — see SUBVOLUME SYNC in the report"
check "report still captured on failure" "$(sed -n 2p <<<"$SUBVOL_SYNC_REPORT")" "  VOLUMES NOT READ"

# A bare call under this file's `set -e`: if the function returned non-zero the
# script would abort here, as backup-run.sh would mid-backup.
sync_subvolumes run
check "bare call under set -e survives a failing sync" "ok" "ok"

# --- sync: config unreadable (exit 2) ---------------------------------------
OP_STATUS=()
printf 'cannot load config\n' >"$WORK/sync_out"; echo 2 >"$WORK/sync_rc"
sync_subvolumes run
check "exit 2 records FAIL" "${OP_STATUS[subvol_sync]}" "FAIL"
check "exit 2 detail names the code" "${OP_STATUS[subvol_sync_detail]}" "exit code 2 — see SUBVOLUME SYNC in the report"

# --- sync: reload failure ---------------------------------------------------
OP_STATUS=()
printf 'SUBVOLUME SYNC\n  nothing to do\n' >"$WORK/sync_out"; echo 0 >"$WORK/sync_rc"
touch "$WORK/env_fail"
verify_calls=0
rc=0; sync_subvolumes run 2>/dev/null || rc=$?
check "failed reload does not re-verify" "$verify_calls" "0"
check "reload failure still returns 0" "$rc" "0"
check "reload failure overrides OK with FAIL" "${OP_STATUS[subvol_sync]}" "FAIL"
check "reload failure detail" "${OP_STATUS[subvol_sync_detail]}" "config could not be reloaded after sync"
# sync failed AND reload failed: both facts are kept.
OP_STATUS=()
printf 'SUBVOLUME SYNC\n  VOLUMES NOT READ\n' >"$WORK/sync_out"; echo 1 >"$WORK/sync_rc"
sync_subvolumes run 2>/dev/null
check "combined failure keeps the exit code and the reload failure" "${OP_STATUS[subvol_sync_detail]}" \
    "exit code 1 — see SUBVOLUME SYNC in the report; config could not be reloaded after sync"
rm -f "$WORK/env_fail"

# --- sync: a failing re-verification is not swallowed ------------------------
echo 0 >"$WORK/sync_rc"
printf 'SUBVOLUME SYNC\n  nothing to do\n' >"$WORK/sync_out"
rc=0
# shellcheck disable=SC2329  # invoked by sync_subvolumes
( verify_sources_before_write() { exit 1; }; sync_subvolumes run; echo "SURVIVED" >"$WORK/survived" ) || rc=$?
check "failing re-verification aborts the run" "$rc" "1"
check "nothing after the aborted sync ran" "$([[ -e "$WORK/survived" ]] && echo yes || echo no)" "no"

# --- sync: an empty report logs nothing -------------------------------------
: >"$WORK/log"; : >"$WORK/sync_out"
# shellcheck disable=SC2329
log_info() { echo "INFO: $*" >>"$WORK/log"; }
sync_subvolumes run
# shellcheck disable=SC2329
log_info() { :; }
check "empty sync report logs no report lines" "$(grep -c '^INFO:   *$' "$WORK/log" || true)" "0"
printf 'SUBVOLUME SYNC\n' >"$WORK/sync_out"

# --- sync: dry run ----------------------------------------------------------
: >"$WORK/calls"; echo 0 >"$WORK/sync_rc"
sync_subvolumes dryrun
check "dry run passes --dry-run" "$(grep -c -- 'subvol sync .*--dry-run' "$WORK/calls")" "1"

# --- expire -----------------------------------------------------------------
: >"$WORK/calls"
printf 'RETIRED SUBVOLUMES\n  @opt\n' >"$WORK/expire_out"; echo 0 >"$WORK/expire_rc"
OP_STATUS[subvol_sync]=OK
expire_retired_subvolumes run
check "clean expire records OK" "${OP_STATUS[subvol_expire]}" "OK"
check "expire passes the db" "$(grep -c -- "--db $DAS_DB_PATH" "$WORK/calls")" "1"
check "real run passes no --dry-run to expire" "$(grep -c -- '--dry-run' "$WORK/calls" || true)" "0"
: >"$WORK/calls"
expire_retired_subvolumes dryrun
check "dry run passes --dry-run to expire" "$(grep -c -- 'subvol expire .*--dry-run' "$WORK/calls")" "1"
echo 1 >"$WORK/expire_rc"
rc=0; expire_retired_subvolumes run || rc=$?
check "failing expire returns 0" "$rc" "0"
check "failing expire records FAIL" "${OP_STATUS[subvol_expire]}" "FAIL"
expire_retired_subvolumes run
check "bare expire call under set -e survives a failing expire" "ok" "ok"
: >"$WORK/expire_out"; echo 0 >"$WORK/expire_rc"
expire_retired_subvolumes run
check "empty expire report is empty" "$SUBVOL_EXPIRE_REPORT" ""

# --- expire after a failed sync: shown, never performed ----------------------
# Sync could not look at the volumes, so a retired subvolume may exist again;
# expiry must not outrun it (Ruling 27).
OP_STATUS=([subvol_sync]=FAIL)
: >"$WORK/calls"
printf 'RETIRED SUBVOLUMES\n  @opt\n    target t: would delete 2 snapshots (window passed)\n' >"$WORK/expire_out"
echo 0 >"$WORK/expire_rc"
expire_retired_subvolumes run
check "failed sync: expire runs as a dry run" "$(grep -c -- 'subvol expire .*--dry-run' "$WORK/calls")" "1"
check "failed sync: expire is recorded as not performed" "${OP_STATUS[subvol_expire]}" "SKIP"
check "failed sync: the detail says why" "${OP_STATUS[subvol_expire_detail]}" \
    "not performed: subvolume sync failed this run (dry run shown)"
check "failed sync: the section says expiry was not performed" \
    "$(sed -n 2p <<<"$SUBVOL_EXPIRE_REPORT")" \
    "  EXPIRY NOT PERFORMED: subvolume sync failed this run; nothing was deleted (dry run shown)"
check "failed sync: the dry-run lines follow" "$(sed -n 3p <<<"$SUBVOL_EXPIRE_REPORT")" "  @opt"
printf 'RETIRED SUBVOLUMES\n' >"$WORK/expire_out"
expire_retired_subvolumes run
check "failed sync, header-only section: note added, nothing else" "$SUBVOL_EXPIRE_REPORT" \
    $'RETIRED SUBVOLUMES\n  EXPIRY NOT PERFORMED: subvolume sync failed this run; nothing was deleted (dry run shown)'
echo 1 >"$WORK/expire_rc"
expire_retired_subvolumes run
check "failed sync and a failing dry-run expire still records FAIL" "${OP_STATUS[subvol_expire]}" "FAIL"
: >"$WORK/expire_out"; echo 0 >"$WORK/expire_rc"
expire_retired_subvolumes run
check "failed sync, nothing retired: no section" "$SUBVOL_EXPIRE_REPORT" ""
check "failed sync, nothing retired: still recorded as not performed" "${OP_STATUS[subvol_expire]}" "SKIP"
# Counter-direction: once sync is OK again, expiry is real.
OP_STATUS=([subvol_sync]=OK)
: >"$WORK/calls"
expire_retired_subvolumes run
check "sync OK: expire is real" "$(grep -c -- '--dry-run' "$WORK/calls" || true)" "0"
check "sync OK: recorded OK" "${OP_STATUS[subvol_expire]}" "OK"

# --- report layout ----------------------------------------------------------
# generate_report's helpers and host probes are stubbed; only the layout of the
# new lines and optional sections is under test.
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
OP_STATUS=([subvol_sync]=OK [subvol_expire]=OK)

SUBVOL_SYNC_REPORT=$'SUBVOLUME SYNC\n  Adopted: @new'
SUBVOL_EXPIRE_REPORT=""
report="$(generate_report)"
check "report lists the sync operation" "$(grep -c '^  Subvolume sync        OK' <<<"$report")" "1"
check "report lists the expiry operation" "$(grep -c '^  Retired expiry        OK' <<<"$report")" "1"
check "sync section is printed" "$(grep -c '^SUBVOLUME SYNC$' <<<"$report")" "1"
check "empty expire report prints no header" "$(grep -c 'RETIRED SUBVOLUMES' <<<"$report" || true)" "0"
check "no double blank line with an empty expire report" "$(grep -c -Pzo '\n\n\n' <<<"$report" || true)" "0"
check "sync section is followed by one blank line, then THROUGHPUT" \
    "$(grep -A2 '^  Adopted: @new$' <<<"$report" | sed -n '2p;3p' | tr '\n' '|')" "|THROUGHPUT|"

SUBVOL_EXPIRE_REPORT=$'RETIRED SUBVOLUMES\n  @opt'
report="$(generate_report)"
check "both sections are printed" "$(grep -c -E '^(SUBVOLUME SYNC|RETIRED SUBVOLUMES)$' <<<"$report")" "2"
check "no double blank line with both sections" "$(grep -c -Pzo '\n\n\n' <<<"$report" || true)" "0"

SUBVOL_SYNC_REPORT=""; SUBVOL_EXPIRE_REPORT=""
report="$(generate_report)"
check "no sections: one blank line before THROUGHPUT" \
    "$(grep -B2 '^THROUGHPUT$' <<<"$report" | sed -n '1,2p' | tr '\n' '|')" "  Retired expiry        OK  (n/a)||"
check "report footer carries the script version" "$(grep -c 'backup-run.sh v4.6.0' <<<"$report")" "1"

if [[ $fails -eq 0 ]]; then
    echo "ALL SUBVOL SYNC SHELL TESTS PASSED"
else
    echo "$fails FAILED"
    exit 1
fi
