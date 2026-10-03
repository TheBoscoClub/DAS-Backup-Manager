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
for fn in load_config_env sync_subvolumes expire_retired_subvolumes record_op any_op_is generate_report run_btrbk cleanup create_target_dirs make_target_dir capture_report_data; do
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
    "subvol sync")
        # --render-btrbk-conf PATH: write the planned btrbk.conf there, as the
        # real command does, when the test supplies one.
        prev=""
        for a in "$@"; do
            if [[ "$prev" == "--render-btrbk-conf" && -f "$here/render_out" ]]; then
                cat "$here/render_out" >"$a"
            fi
            prev="$a"
        done
        cat "$here/sync_out"; exit "$(cat "$here/sync_rc")" ;;
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

# --- dry run: btrbk dryrun reads the btrbk.conf sync plans (g17) ------------
# A stub btrbk records which config it was given, and whether that file still
# exists at the time; the real btrbk.conf must never be what a dry run reads
# while sync has a plan.
# shellcheck disable=SC2329  # called by the extracted run_btrbk
btrbk() { echo "$*" >>"$WORK/btrbk_calls"; [[ "$2" == "$WORK"/* && -f "$2" ]] && cp "$2" "$WORK/btrbk_saw"; return 0; }
# The config reload inside sync resets DAS_BTRBK_CONF from the stub's
# dump-env; the file below stands for the real one and must not change.
REAL_CONF="$WORK/btrbk.conf"
echo "REAL CONF WITH @gone" >"$REAL_CONF"
export TMPDIR="$WORK/tmp"; mkdir -p "$TMPDIR"
real_sum="$(sha256sum "$REAL_CONF")"

# Pending retirement: sync plans a btrbk.conf without the dead entry.
: >"$WORK/calls"; : >"$WORK/btrbk_calls"; rm -f "$WORK/btrbk_saw"
printf 'SUBVOLUME SYNC\n  Would retire (dry run, nothing written):\n    @gone\n' >"$WORK/sync_out"; echo 0 >"$WORK/sync_rc"
echo "PLANNED CONF WITHOUT @gone" >"$WORK/render_out"
DRYRUN_BTRBK_CONF=""
sync_subvolumes dryrun
check "dry run asks sync to render the planned btrbk.conf" \
    "$(grep -c -- 'subvol sync .*--dry-run --render-btrbk-conf '"$TMPDIR"'/das-backup-dryrun-btrbk\.' "$WORK/calls")" "1"
check "the temp file is mode 600" "$(stat -c %a "$DRYRUN_BTRBK_CONF")" "600"
run_btrbk dryrun
check "btrbk dryrun is given the temp conf" "$(cat "$WORK/btrbk_calls")" "-c $DRYRUN_BTRBK_CONF dryrun"
check "and it held the planned config" "$(cat "$WORK/btrbk_saw")" "PLANNED CONF WITHOUT @gone"
check "the real btrbk.conf is byte-identical" "$(sha256sum "$REAL_CONF")" "$real_sum"
planned_file="$DRYRUN_BTRBK_CONF"
# shellcheck disable=SC2034  # read by the extracted cleanup
( SCRIPT_COMPLETED="true"; cleanup ) || true
check "the EXIT trap removes the temp conf" "$([[ -e "$planned_file" ]] && echo yes || echo no)" "no"

# Nothing pending: the planned file is just the current one; still works.
: >"$WORK/btrbk_calls"; DRYRUN_BTRBK_CONF=""
printf 'SUBVOLUME SYNC\n  No new, vanished or returning subvolumes.\n' >"$WORK/sync_out"
cp "$REAL_CONF" "$WORK/render_out"
sync_subvolumes dryrun
run_btrbk dryrun
check "nothing pending: btrbk still reads a temp conf" "$(grep -c -- "-c $TMPDIR/das-backup-dryrun-btrbk\." "$WORK/btrbk_calls")" "1"
check "nothing pending: btrbk dryrun recorded OK" "${OP_STATUS[btrbk]}" "OK"
# shellcheck disable=SC2034  # read by the extracted cleanup
( SCRIPT_COMPLETED="true"; cleanup ) || true

# Sync rendered nothing (no readable btrbk.conf would be left): fall back to
# the current file, and leave no empty temp file behind.
: >"$WORK/btrbk_calls"; DRYRUN_BTRBK_CONF=""; rm -f "$WORK/render_out"; : >"$WORK/log"
sync_subvolumes dryrun
check "no render: the variable is cleared" "$DRYRUN_BTRBK_CONF" ""
check "no render: no temp file is left" "$(find "$TMPDIR" -name 'das-backup-dryrun-btrbk.*' | wc -l)" "0"
check "no render: it is said" "$(grep -c 'rendered no btrbk.conf' "$WORK/log")" "1"
run_btrbk dryrun
check "no render: btrbk dryrun reads the current conf" "$(cat "$WORK/btrbk_calls")" "-c $DAS_BTRBK_CONF dryrun"

# A real run never renders and never uses a temp conf.
: >"$WORK/calls"; : >"$WORK/btrbk_calls"; DRYRUN_BTRBK_CONF=""
echo "PLANNED" >"$WORK/render_out"
sync_subvolumes run
check "real run: no --render-btrbk-conf" "$(grep -c -- '--render-btrbk-conf' "$WORK/calls" || true)" "0"
check "real run: no temp file made" "$(find "$TMPDIR" -name 'das-backup-dryrun-btrbk.*' | wc -l)" "0"
run_btrbk run
check "real run: btrbk run reads the real conf" "$(cat "$WORK/btrbk_calls")" "-c $DAS_BTRBK_CONF run"
rm -f "$WORK/render_out"
unset -f btrbk
unset TMPDIR

# --- dry run: a pending adoption's new target directory ------------------------
# The planned btrbk.conf sends @new to <mnt>/nvme-adopted, which the current
# config does not name. The real run creates it after its reload; the dry run
# must too, or btrbk dryrun fails on it. Only under a mounted target.
# shellcheck disable=SC2329  # called by the extracted create_target_dirs
mountpoint() { [[ "${!#}" == "$WORK/mnt/t0" ]]; }
mkdir -p "$WORK/mnt/t0" "$WORK/mnt/t1"
# Read by the extracted create_target_dirs through indirect expansion.
# shellcheck disable=SC2034
DAS_SOURCE_COUNT=1
# shellcheck disable=SC2034
DAS_SOURCE_0_TARGET_SUBDIRS="nvme"
# shellcheck disable=SC2034
DAS_TARGET_COUNT=2
# shellcheck disable=SC2034
DAS_TARGET_0_MOUNT="$WORK/mnt/t0"
# shellcheck disable=SC2034
DAS_TARGET_1_MOUNT="$WORK/mnt/t1"
printf 'volume /.btrfs-nvme\n  target                %s\n  target                %s\n  target                %s\n  target                %s\n' \
    "$WORK/mnt/t0/nvme" "$WORK/mnt/t0/nvme-adopted" "$WORK/mnt/t1/nvme-adopted" "$WORK/mnt/t0/../escape" >"$WORK/planned.conf"
DRYRUN_BTRBK_CONF=""
create_target_dirs
check "no plan: only the configured subdir" "$(find "$WORK/mnt/t0" -mindepth 1 -printf '%f ')" "nvme "
DRYRUN_BTRBK_CONF="$WORK/planned.conf"
: >"$WORK/log"
# shellcheck disable=SC2329  # called by the extracted make_target_dir
log_info() { echo "INFO: $*" >>"$WORK/log"; }
create_target_dirs
check "plan: the adopted subdir on the mounted target" "$([[ -d "$WORK/mnt/t0/nvme-adopted" ]] && echo yes || echo no)" "yes"
check "plan: the directory it created is logged" "$(grep -c "Created target directory $WORK/mnt/t0/nvme-adopted" "$WORK/log")" "1"
check "plan: an existing directory is not logged again" "$(grep -c "Created target directory $WORK/mnt/t0/nvme\$" "$WORK/log" || true)" "0"
# shellcheck disable=SC2329
log_info() { :; }
check "main: target directories are made only after the targets are verified" \
    "$(awk '/^main\(\) \{/,/^}/' "$SCRIPT" | grep -n -E '^    (verify_targets_before_btrbk|create_target_dirs)$' | cut -d: -f2 | tr -d ' ' | tr '\n' ' ')" \
    "verify_targets_before_btrbk create_target_dirs "
check "plan: nothing on an unmounted target" "$(find "$WORK/mnt/t1" -mindepth 1 | wc -l)" "0"
check "plan: no path that climbs out of the target" "$([[ -e "$WORK/mnt/escape" ]] && echo yes || echo no)" "no"
DRYRUN_BTRBK_CONF=""
unset -f mountpoint

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
# shellcheck disable=SC2034
RECOVERY_OS_REPORT=""
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
    "$(grep -B2 '^THROUGHPUT$' <<<"$report" | sed -n '1,2p' | tr '\n' '|')" "  Recovery OS           N/A  (n/a)||"
check "report footer carries the script version" "$(grep -c 'backup-run.sh v4.8.0' <<<"$report")" "1"

# --- report: a failed btrbk listing is unavailable, not "none yet" -------------
# capture_report_data under the script's own options; no target is mounted.
# shellcheck disable=SC2329  # called by the extracted capture_report_data
mountpoint() { return 1; }
# shellcheck disable=SC2034  # read by the extracted capture_report_data
ALL_TARGET_MOUNTS=()
latest_case() {
    # shellcheck disable=SC2329
    btrbk() {
        if [[ "$*" == *"--format=raw"* ]]; then return 0; fi
        case "$2" in
            ok) printf 'SOURCE SNAPSHOT\n/v/@ /v/.s/root-.1\n' ;;
            empty) printf 'SOURCE SNAPSHOT\n' ;;
            fail) echo "ERROR: Failed to lock" >&2; return 2 ;;
        esac
    }
    DAS_BTRBK_CONF="$1"
    ( set -euo pipefail; capture_report_data; printf '%s' "$BTRBK_LATEST" )
}
check "report: a listing is shown" "$(latest_case ok)" "  /v/@ /v/.s/root-.1"
check "report: an empty listing stays empty (shown as none yet)" "$(latest_case empty)" ""
check "report: a failed listing says so and why" "$(latest_case fail)" \
    "  (unavailable: btrbk list latest failed: ERROR: Failed to lock)"
unset -f btrbk mountpoint

if [[ $fails -eq 0 ]]; then
    echo "ALL SUBVOL SYNC SHELL TESTS PASSED"
else
    echo "$fails FAILED"
    exit 1
fi
