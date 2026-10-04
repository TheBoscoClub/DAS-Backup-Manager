#!/bin/bash
# shellcheck disable=SC2016
# SC2016: the single-quoted text below is the source of stub programs and the
#   literal text of lines in backup-run.sh; expanding it would defeat both.
#
# tests/test_backup_exit_semantics.sh
#
# The exit status of scripts/backup-run.sh — operator decision C, bd
# DAS-Backup-Manager-d1r (2026-10-04), the doctor's rule:
#
#   0        the run executed and nothing FAILED (a WARN still exits 0)
#   3        the run began its work and something FAILED or it aborted: btrbk
#            nonzero for some or all targets, any FAIL operation, an abort on
#            a target's or a source's state. Both unit sources and the units
#            `btrdasd setup` writes list SuccessExitStatus=3, so systemd and
#            cachyos-sentinel see success and never restart the run, while
#            the journal still shows status=3.
#   1        it could not start: config unreadable, btrdasd missing, an
#            argument error, not root, the maintenance lock unusable.
#            Nothing was mounted or sent.
#   0        a skip: another backup holds the singleton lock (unchanged).
#   130/143  stopped by SIGINT/SIGTERM (unchanged).
#
# The defect (measured 2026-10-02, a run by hand with recovery drive A
# pulled): btrbk exits 10 when any one target aborts, and the script turned
# that into exit 1. Under the unit that is `failed` on every run, and
# cachyos-sentinel — 3 restarts per 600 s, so it cannot brake a loop slower
# than ten minutes — would start a whole new backup each time, one roughly
# every ten minutes until the drive came back.
#
# The singleton lock (bd DAS-Backup-Manager-ismb): only "held by another run"
# is a skip (0); a lock file that cannot be opened, or a flock that fails for
# any other reason, is "could not start" (1), and says why.
#
# How: the REAL script runs end to end — its EXIT trap, cleanup() and its
# `main "$@"; exit $?` line included — from a copy in which exactly three lines
# differ: the two lock paths point into a temp dir instead of /run, and the
# root check reads DAS_TEST_EUID. It runs under `env -i` with a PATH of stubs
# (btrdasd, btrbk, mount, umount, mountpoint, findmnt, blkid, btrfs, smartctl,
# df, systemctl, mailx) and a whitelist of harmless tools only: a command the
# stubs do not cover is "command not found", which fails the case. Nothing
# real can be mounted, sent or locked. The mount, umount, mountpoint and
# findmnt stubs share one mount table (a file), so what the script mounts it
# can verify, and what cleanup() leaves mounted the test can see.
#
# Writes only beneath a mktemp directory. No root, no devices, no network.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$REPO_ROOT/scripts/backup-run.sh"
# Every path this suite writes or removes is under $WORK. Without it, stop:
# this file runs without `set -e`, and fresh() removes paths built from it.
WORK="$(mktemp -d)" && [[ -d "$WORK" ]] || {
    echo "HARNESS BROKEN: no temp dir"
    exit 2
}
HOLDER_PID=""
finish() {
    [[ -n "$HOLDER_PID" ]] && kill "$HOLDER_PID" 2>/dev/null && wait "$HOLDER_PID" 2>/dev/null
    rm -rf "${WORK:?}"
}
trap finish EXIT

STATE="$WORK/state"
RUN_DIR="$WORK/run"
COPY="$WORK/backup-run.sh"
BIN="$WORK/bin"
SYSBIN="$WORK/sysbin"

pass=0
fail=0
harness_broken() {
    echo "HARNESS BROKEN: $*"
    exit 2
}
# "ok"/"FAIL" as the sibling suites print them.
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

# ---------------------------------------------------------------------------
# The sandboxed copy
# ---------------------------------------------------------------------------
LOCK_LINE='LOCKFILE="/run/das-backup.lock"'
MAINT_LINE='MAINTENANCE_LOCKFILE="/run/das-maintenance.lock"'
ROOT_LINE='    if [[ $EUID -ne 0 ]]; then'
for line in "$LOCK_LINE" "$MAINT_LINE" "$ROOT_LINE"; do
    n="$(grep -cxF -- "$line" "$SRC")"
    [[ "$n" == 1 ]] || harness_broken "expected exactly one line '$line' in $SRC, found $n"
done
sed -e "s|^LOCKFILE=\"/run/das-backup.lock\"\$|LOCKFILE=\"$RUN_DIR/das-backup.lock\"|" \
    -e "s|^MAINTENANCE_LOCKFILE=\"/run/das-maintenance.lock\"\$|MAINTENANCE_LOCKFILE=\"$RUN_DIR/das-maintenance.lock\"|" \
    -e 's|^    if \[\[ \$EUID -ne 0 \]\]; then$|    if [[ ${DAS_TEST_EUID:?} -ne 0 ]]; then|' \
    "$SRC" >"$COPY"
changed="$(diff "$SRC" "$COPY" | grep -c '^>')"
[[ "$changed" == 3 ]] || harness_broken "the copy differs from $SRC in $changed lines, not 3"
# The guard that matters: nothing outside a comment may still name the real
# locks. If this ever fires, the copy is never run.
if grep -v '^[[:space:]]*#' "$COPY" | grep -q '/run/das-'; then
    harness_broken "the copy still names a /run/das- path outside a comment"
fi

# ---------------------------------------------------------------------------
# PATH: stubs, and a whitelist of harmless tools
# ---------------------------------------------------------------------------
mkdir -p "$BIN" "$SYSBIN"
for tool in awk basename cat cut date dirname flock grep head hostname mkdir mktemp mv \
    rm rmdir sed sleep sort tail timeout touch tr wc; do
    path="$(type -P "$tool")" || harness_broken "no $tool on this system"
    ln -s "$path" "$SYSBIN/$tool"
done
# The flock stub hands every call it does not fake to this one.
REAL_FLOCK="$(type -P flock)"

stub() { # stub <name>: the program text on stdin, with the common preamble
    {
        echo '#!/bin/bash'
        echo 'S="$DAS_TEST_STATE"'
        echo 'knob() { if [[ -f "$S/knobs/$1" ]]; then cat "$S/knobs/$1"; else echo "$2"; fi; }'
        echo 'listed() { [[ -f "$S/knobs/$1" ]] && grep -qxF -- "$2" "$S/knobs/$1"; }'
        # The \t is for awk, so it is written out as it stands.
        printf '%s\n' 'is_mounted() { awk -F"\t" -v p="$1" '\''$1 == p { f = 1 } END { exit !f }'\'' "$S/mounted"; }'
        cat
    } >"$BIN/$1"
    chmod +x "$BIN/$1"
}

stub btrdasd <<'EOF'
printf '%s\n' "$*" >>"$S/calls/btrdasd"
case "$1 ${2:-}" in
"config dump-env")
    rc="$(knob dump_env_rc 0)"
    if [[ "$rc" != 0 ]]; then echo "stub: cannot read the config" >&2; exit "$rc"; fi
    cat "$S/env"
    ;;
"subvol sync")
    printf 'SUBVOLUME SYNC\n  No new, vanished or returning subvolumes.\n'
    # The run's log file stops being writable: it becomes a directory.
    if [[ -f "$S/knobs/break_log" ]]; then
        log="$(cat "$S/knobs/break_log")"
        rm -f "$log"
        mkdir "$log"
    fi
    while [[ $# -gt 0 ]]; do
        [[ "$1" == --render-btrbk-conf ]] && echo "# planned by the stub" >"$2"
        shift
    done
    exit "$(knob sync_rc 0)"
    ;;
"subvol expire")
    printf 'RETIRED SUBVOLUMES\n  None.\n'
    exit "$(knob expire_rc 0)"
    ;;
"recovery-os status")
    printf 'RECOVERY OS\n  stub reading\n'
    exit "$(knob recovery_rc 0)"
    ;;
"backup record-run")
    printf '%s\n' "$@" >"$S/record_args"
    exit "$(knob record_rc 0)"
    ;;
walk*)
    echo "Discovered: 0 new snapshots"
    exit "$(knob walk_rc 0)"
    ;;
*)
    echo "btrdasd stub: unexpected: $*" >&2
    exit 99
    ;;
esac
EOF

stub btrbk <<'EOF'
printf '%s\n' "$*" >>"$S/calls/btrbk"
case " $* " in
*" --format=raw list latest "*)
    echo "format=\"latest\" snapshot_subvolume='/v/.btrbk-snapshots/root-.20261004T0300' target_subvolume='/t/nvme/root-.20261004T0300'"
    ;;
*" list latest "*)
    echo "SOURCE_SUBVOLUME SNAPSHOT_SUBVOLUME STATUS TARGET_SUBVOLUME"
    echo "/v/@ /v/.btrbk-snapshots/root-.20261004T0300 - /t/nvme/root-.20261004T0300"
    ;;
*" run "* | *" dryrun "*)
    mode=run
    [[ " $* " == *" dryrun "* ]] && mode=dryrun
    if [[ -f "$S/knobs/btrbk_blocks" ]]; then
        : >"$S/btrbk_started"
        sleep 30
    fi
    exit "$(knob "btrbk_${mode}_rc" 0)"
    ;;
*)
    echo "btrbk stub: unexpected: $*" >&2
    exit 99
    ;;
esac
EOF

stub mount <<'EOF'
printf '%s\n' "$*" >>"$S/calls/mount"
src="${*: -2:1}"
dst="${*: -1}"
if listed mount_fails "$dst"; then
    echo "mount: $dst: wrong fs type, bad option, bad superblock (stub)" >&2
    exit 32
fi
uuid="${src#UUID=}"
listed wrong_fs_at "$dst" && uuid="a-different-filesystem"
printf '%s\t%s\t/\n' "$dst" "$uuid" >>"$S/mounted"
EOF

stub umount <<'EOF'
printf '%s\n' "$*" >>"$S/calls/umount"
dst="${*: -1}"
if listed umount_fails_once "$dst" && ! listed umount_failed "$dst"; then
    echo "$dst" >>"$S/knobs/umount_failed"
    echo "umount: $dst: target is busy (stub)." >&2
    exit 32
fi
is_mounted "$dst" || { echo "umount: $dst: not mounted (stub)." >&2; exit 32; }
awk -F'\t' -v p="$dst" '$1 != p' "$S/mounted" >"$S/mounted.new"
mv "$S/mounted.new" "$S/mounted"
EOF

stub mountpoint <<'EOF'
is_mounted "${*: -1}"
EOF

stub findmnt <<'EOF'
cols="" target=""
while [[ $# -gt 0 ]]; do
    case "$1" in
    -o) cols="$2"; shift ;;
    --target) target="$2"; shift ;;
    esac
    shift
done
# Like the real findmnt, a path that is not a mount point reports the
# filesystem that contains it — here the root filesystem.
uuid="root-filesystem-uuid" fsroot="/" source="/nonexistent/root-device"
line="$(awk -F'\t' -v p="$target" '$1 == p { print; exit }' "$S/mounted")"
if [[ -n "$line" ]]; then
    IFS=$'\t' read -r _ uuid fsroot <<<"$line"
    source="/nonexistent/das-test-device1"
fi
case "$cols" in
UUID,FSROOT) echo "$uuid $fsroot" ;;
UUID) echo "$uuid" ;;
SOURCE) echo "$source" ;;
*) echo "findmnt stub: unexpected -o $cols" >&2; exit 1 ;;
esac
EOF

stub blkid <<'EOF'
if [[ "${1:-}" == -U ]] && listed present_uuids "${2:-}"; then
    # Not a device node, so set_io_scheduler and the SMART section skip it.
    echo "/nonexistent/das-test-$2-1"
    exit 0
fi
exit 2
EOF

stub smartctl <<'EOF'
exit 0
EOF

stub btrfs <<'EOF'
case "$1 ${2:-}" in
"filesystem label") echo "stub-label" ;;
"subvolume list") exit 0 ;;
*) echo "btrfs stub: unexpected: $*" >&2; exit 1 ;;
esac
EOF

stub df <<'EOF'
case " $* " in
*" --output=used "*) printf 'Used\n1000\n' ;;
*" --output=avail "*) printf 'Avail\n5000\n' ;;
*" -h "*) printf 'Filesystem Size Used Avail Use%% Mounted on\nstub 10G 1.0K 5.0K 1%% %s\n' "${*: -1}" ;;
*) echo "df stub: unexpected: $*" >&2; exit 1 ;;
esac
EOF

stub systemctl <<'EOF'
echo "NextElapseUSecRealtime="
EOF

stub mailx <<'EOF'
cat >/dev/null
printf '%s\n' "$*" >>"$S/calls/mailx"
EOF

# flock: fd 9 (the singleton lock) fails with the status in the flock9_rc
# knob, as flock does for ENOLCK (71) — or as a flock that answers 1 to any
# failure would; every other call is the real flock.
stub flock <<'EOF'
if [[ -f "$S/knobs/flock9_rc" && "${*: -1}" == 9 ]]; then
    echo "flock: 9: No locks available (stub)" >&2
    exit "$(cat "$S/knobs/flock9_rc")"
fi
exec "$(cat "$S/real_flock")" "$@"
EOF

stub boot-archive-cleanup.sh <<'EOF'
echo "[das-backup-22tb] Deleted 0, kept 0, errors 0"
EOF

# ---------------------------------------------------------------------------
# One run's sandbox: a config, a mount table, knobs
# ---------------------------------------------------------------------------
PRIMARY_MNT="$WORK/mnt/backup-22tb"
RECOVERY_MNT="$WORK/mnt/backup-system-recovery-A"
SOURCE_MNT="$WORK/mnt/btrfs-nvme"

# What `btrdasd config dump-env` prints for one source and two targets: the
# RAID-1 primary and one recovery drive, both mounted by UUID.
write_env() {
    cat >"$STATE/env" <<EOF
DAS_DB_PATH='$WORK/lib/backup-index.db'
DAS_LOG_FILE='$WORK/log/das-backup.log'
DAS_GROWTH_LOG='$WORK/lib/growth.log'
DAS_LAST_REPORT='$WORK/lib/last-report.txt'
DAS_BTRBK_CONF='$WORK/etc/btrbk.conf'
DAS_IO_SCHEDULER='mq-deadline'
DAS_MOUNT_OPTS='noatime,degraded'
DAS_SOURCE_COUNT=1
DAS_SOURCE_0_LABEL='nvme'
DAS_SOURCE_0_VOLUME='$SOURCE_MNT'
DAS_SOURCE_0_DEVICE='UUID=source-uuid'
DAS_SOURCE_0_SUBVOLUMES='@ @home'
DAS_SOURCE_0_SNAPSHOT_DIR='.btrbk-snapshots'
DAS_SOURCE_0_TARGET_SUBDIRS='nvme'
DAS_TARGET_COUNT=2
DAS_TARGET_0_LABEL='primary-22tb'
DAS_TARGET_0_SERIAL='SERIALP1'
DAS_TARGET_0_SERIALS='SERIALP1 SERIALP2'
DAS_TARGET_0_MOUNT_UUID='primary-uuid'
DAS_TARGET_0_MOUNT='$PRIMARY_MNT'
DAS_TARGET_0_ROLE='primary'
DAS_TARGET_0_DISPLAY_NAME='Primary 22TB'
DAS_TARGET_1_LABEL='system-recovery-A-2tb'
DAS_TARGET_1_SERIAL='SERIALA1'
DAS_TARGET_1_SERIALS='SERIALA1'
DAS_TARGET_1_MOUNT_UUID='recovery-a-uuid'
DAS_TARGET_1_MOUNT='$RECOVERY_MNT'
DAS_TARGET_1_ROLE='mirror'
DAS_ALL_TARGET_MOUNTS='$PRIMARY_MNT $RECOVERY_MNT'
DAS_EMAIL_ENABLED=false
EOF
}

# A new sandbox: nothing mounted, both targets present, every knob at its
# default (every stub succeeds).
fresh() {
    rm -rf "${STATE:?}" "${WORK:?}/mnt" "${WORK:?}/lib" "${WORK:?}/log" "${WORK:?}/etc" \
        "${RUN_DIR:?}" "${WORK:?}/tmp" "${WORK:?}/home"
    mkdir -p "$STATE/knobs" "$STATE/calls" "$WORK/mnt" "$WORK/etc" "$RUN_DIR" "$WORK/tmp" "$WORK/home"
    : >"$STATE/mounted"
    : >"$WORK/etc/btrbk.conf"
    printf '%s\n' "$REAL_FLOCK" >"$STATE/real_flock"
    printf '%s\n' primary-uuid recovery-a-uuid >"$STATE/knobs/present_uuids"
    write_env
}
knob() { printf '%s\n' "$2" >"$STATE/knobs/$1"; }

RUN_EUID=0
RUN_BTRDASD="$BIN/btrdasd"
RC=""
# The command line of one run, into CMD: the copy, under `env -i`.
run_cmd() {
    CMD=(env -i
        PATH="$BIN:$SYSBIN" HOME="$WORK/home" LC_ALL=C TMPDIR="$WORK/tmp"
        DAS_TEST_STATE="$STATE" DAS_TEST_EUID="$RUN_EUID"
        BTRDASD_BIN="$RUN_BTRDASD" DAS_CONFIG="$WORK/etc/config.toml"
        BOOT_ARCHIVE_CLEANUP_BIN="$BIN/boot-archive-cleanup.sh"
        DAS_RECOVERY_OS_STATE="$WORK/lib/recovery-os.json"
        "$BASH" "$COPY" "$@")
}
tripwire() {
    if grep -q 'command not found' "$STATE/out"; then
        bad "the run reached a command that is neither stubbed nor whitelisted: $(grep -m1 'command not found' "$STATE/out")"
    fi
}
# run_backup [args...]: one run, to completion; its status in RC.
run_backup() {
    run_cmd "$@"
    "${CMD[@]}" >"$STATE/out" 2>&1
    RC=$?
    tripwire
}

called() { [[ -s "$STATE/calls/$1" ]] && echo yes || echo no; }
ran_btrbk() { grep -qE '(^| )(run|dryrun)$' "$STATE/calls/btrbk" 2>/dev/null && echo yes || echo no; }
left_mounted() {
    local m
    m="$(cut -f1 "$STATE/mounted" | tr '\n' ' ')"
    echo "${m:-nothing}"
}
report_status() { sed -n 's/^  Status: //p' "$WORK/lib/last-report.txt" 2>/dev/null | head -n1; }
recorded_as() {
    if [[ ! -f "$STATE/record_args" ]]; then
        echo "not recorded"
    elif grep -qx -- '--success' "$STATE/record_args"; then
        echo "success"
    else
        echo "failure"
    fi
}
show_tail() { [[ "$RC" == "$1" ]] || sed 's/^/      | /' "$STATE/out" | tail -n 15; }

# A run that reached its report: the exit status, the report's status line,
# the history row, and nothing left mounted.
expect_completed() { # expect_completed <name> <status> <report status> <recorded as>
    check "$1: exit status" "$RC" "$2"
    show_tail "$2"
    check "$1: report status" "$(report_status)" "$3"
    check "$1: history row" "$(recorded_as)" "$4"
    check "$1: nothing left mounted" "$(left_mounted)" "nothing"
}
# A run that aborted once it held the maintenance lock: 3, btrbk never ran,
# cleanup() unmounted what the run had mounted.
expect_aborted() { # expect_aborted <name> <what the log must say>
    check "$1: exit status" "$RC" "3"
    show_tail "3"
    check "$1: btrbk never ran" "$(ran_btrbk)" "no"
    check "$1: says why" "$(grep -cF -- "$2" "$STATE/out")" "1"
    check "$1: cleanup() ran its recovery body" \
        "$(grep -c 'Cleaning up after abnormal termination' "$STATE/out")" "1"
    check "$1: nothing left mounted" "$(left_mounted)" "nothing"
}
# A run that could not start: 1, and nothing was mounted or sent. Nor was
# anything unmounted: without the maintenance lock cleanup() must not touch
# the targets a scrub may hold (bd DAS-Backup-Manager-oeo).
expect_not_started() { # expect_not_started <name>
    check "$1: exit status" "$RC" "1"
    show_tail "1"
    check "$1: nothing mounted" "$(called mount)" "no"
    check "$1: btrbk never ran" "$(ran_btrbk)" "no"
    check "$1: cleanup() skipped its recovery body" \
        "$(grep -c 'Cleaning up after abnormal termination' "$STATE/out")" "0"
    check "$1: nothing unmounted" "$(called umount)" "no"
}

# ---------------------------------------------------------------------------
echo "== 0: the run executed and nothing failed"
# ---------------------------------------------------------------------------
fresh
run_backup
expect_completed "clean run" 0 "ALL OPERATIONS SUCCESSFUL" success
check "clean run: btrbk ran" "$(ran_btrbk)" "yes"

fresh
run_backup --full
expect_completed "clean full run" 0 "ALL OPERATIONS SUCCESSFUL" success

fresh
knob recovery_rc 1 # a recovery OS behind the host: WARN
run_backup
expect_completed "warnings only (a stale recovery OS)" 0 "COMPLETED WITH WARNINGS" success

fresh
run_backup --dryrun
check "clean dry run: exit status" "$RC" "0"
show_tail 0
check "clean dry run: btrbk dryrun ran" "$(ran_btrbk)" "yes"
check "clean dry run: nothing left mounted" "$(left_mounted)" "nothing"

# ---------------------------------------------------------------------------
echo "== 3: the run began its work and something FAILED"
# ---------------------------------------------------------------------------
fresh
knob btrbk_run_rc 10 # btrbk: at least one target aborted
run_backup
expect_completed "btrbk partial failure (btrbk exits 10)" 3 "FAILURES DETECTED" failure
check "btrbk partial failure: btrbk ran" "$(ran_btrbk)" "yes"

# The live case of 2026-10-02: one recovery drive absent, btrbk aborts the
# target it cannot reach and exits 10. The other target was backed up.
fresh
printf '%s\n' primary-uuid >"$STATE/knobs/present_uuids"
knob btrbk_run_rc 10
run_backup
expect_completed "recovery drive absent, btrbk exits 10" 3 "FAILURES DETECTED" failure
check "recovery drive absent: the primary was mounted and backed up" \
    "$(grep -c -- "UUID=primary-uuid $PRIMARY_MNT" "$STATE/calls/mount")" "1"

fresh
knob btrbk_run_rc 1 # btrbk failed outright
run_backup
expect_completed "btrbk total failure (btrbk exits 1)" 3 "FAILURES DETECTED" failure

fresh
knob btrbk_run_rc 2 # btrbk could not parse its config
run_backup
expect_completed "btrbk total failure (btrbk exits 2)" 3 "FAILURES DETECTED" failure

fresh
knob walk_rc 1
run_backup
expect_completed "a FAIL without btrbk failing (indexer)" 3 "FAILURES DETECTED" failure
check "indexer FAIL: btrbk itself succeeded" "$(grep -c 'btrbk completed' "$STATE/out")" "1"

fresh
knob sync_rc 1
run_backup
expect_completed "a FAIL without btrbk failing (subvolume sync)" 3 "FAILURES DETECTED" failure

fresh
knob recovery_rc 2 # the check itself failed: FAIL, not WARN
run_backup
expect_completed "a FAIL without btrbk failing (recovery OS check)" 3 "FAILURES DETECTED" failure

# The history row is written after the report, and a record that fails is a
# FAIL (bd 6wt): the report is sent again, and the run exits 3.
fresh
knob record_rc 2
run_backup
check "a FAIL without btrbk failing (history record): exit status" "$RC" "3"
show_tail 3
check "history record failed: the report sent again says so" "$(report_status)" "FAILURES DETECTED"

fresh
knob btrbk_dryrun_rc 10
run_backup --dryrun
check "dry run, btrbk dryrun exits 10: exit status" "$RC" "3"
show_tail 3

# ---------------------------------------------------------------------------
echo "== 3: the run began its work and aborted on a target's or a source's state"
# ---------------------------------------------------------------------------
fresh
knob wrong_fs_at "$PRIMARY_MNT"
run_backup
expect_aborted "verify_targets_before_btrbk (wrong filesystem on a target)" \
    "ABORTING — refusing to invoke btrbk"

fresh
knob mount_fails "$RECOVERY_MNT"
run_backup
expect_aborted "a target that fails to mount" "is NOT a mountpoint (mount failed silently in mount_targets)"

fresh
printf '%s\n' primary-uuid >"$STATE/knobs/present_uuids"
mkdir -p "$RECOVERY_MNT"
echo "written while the drive was away" >"$RECOVERY_MNT/stray-file"
run_backup
expect_aborted "the bare-mountpoint guard (absent target, non-empty directory)" \
    "ABORTING: target system-recovery-A-2tb is unavailable but $RECOVERY_MNT is non-empty"

fresh
printf '%s\n' primary-uuid >"$STATE/knobs/present_uuids"
mkdir -p "$RECOVERY_MNT"
printf '%s\trecovery-a-uuid\t/\n' "$RECOVERY_MNT" >>"$STATE/mounted"
knob umount_fails_once "$RECOVERY_MNT"
run_backup
expect_aborted "an absent target still mounted that will not unmount" \
    "refusing to proceed — $RECOVERY_MNT is mounted but target is marked unavailable"

fresh
printf '%s\n' recovery-a-uuid >"$STATE/knobs/present_uuids"
run_backup
expect_aborted "no primary target available" "No primary backup target is available — aborting"

fresh
knob mount_fails "$SOURCE_MNT"
run_backup
# Not an explicit exit in the script: `mount` fails (32) under set -e, and
# cleanup() turns any status of a run that held the lock into 3.
expect_aborted "a source that fails to mount (set -e, mount exits 32)" "wrong fs type, bad option"

fresh
knob wrong_fs_at "$SOURCE_MNT"
run_backup
expect_aborted "verify_sources_before_write (wrong filesystem on a source)" \
    "ABORTING — refusing to write to source volumes"

# The log file stops being writable mid-run (the root filesystem full, say):
# the next log line aborts the run under set -e, and cleanup()'s own log
# lines fail too. They must not end the trap early with their status (1) —
# the run still exits 3 and still unmounts what it mounted.
fresh
knob break_log "$WORK/log/das-backup.log"
run_backup
check "the log unwritable mid-run: exit status" "$RC" "3"
show_tail 3
check "the log unwritable mid-run: cleanup() ran its recovery body" \
    "$(grep -c 'Cleaning up after abnormal termination' "$STATE/out")" "1"
check "the log unwritable mid-run: nothing left mounted" "$(left_mounted)" "nothing"

# ---------------------------------------------------------------------------
echo "== 1: it could not start — nothing mounted, nothing sent"
# ---------------------------------------------------------------------------
fresh
knob dump_env_rc 1
run_backup
expect_not_started "config unreadable"

fresh
RUN_BTRDASD="$WORK/no-such-btrdasd"
run_backup
RUN_BTRDASD="$BIN/btrdasd"
expect_not_started "btrdasd missing"

fresh
run_backup --no-such-option
expect_not_started "argument error"

fresh
RUN_EUID=1000
run_backup
RUN_EUID=0
expect_not_started "not root"

fresh
mkdir -p "$RUN_DIR/das-maintenance.lock" # a lock file that cannot be opened
run_backup
expect_not_started "the maintenance lock cannot be opened"

# bd DAS-Backup-Manager-ismb: the singleton lock. Only "another run holds it"
# is a skip; a lock that cannot be taken at all is "could not start", and the
# run says why. Each of these used to exit 0 as a skip — every backup silently
# disabled for as long as the lock file stayed broken.
fresh
mkdir -p "$RUN_DIR/das-backup.lock" # the lock file cannot be opened
run_backup
expect_not_started "the singleton lock file cannot be opened"
check "singleton unopenable: says why" \
    "$(grep -c "Cannot open the backup lock $RUN_DIR/das-backup.lock — could not start" "$STATE/out")" "1"
check "singleton unopenable: no skip claimed" "$(grep -c 'skipping this invocation' "$STATE/out")" "0"
check "singleton unopenable: the config was never read" "$(called btrdasd)" "no"

fresh
knob flock9_rc 71 # ENOLCK, as util-linux flock reports it
run_backup
expect_not_started "the singleton lock cannot be taken (flock exits 71)"
check "flock 71: says why" \
    "$(grep -c "Cannot lock $RUN_DIR/das-backup.lock (flock exit 71) — could not start" "$STATE/out")" "1"
check "flock 71: no skip claimed" "$(grep -c 'skipping this invocation' "$STATE/out")" "0"

fresh
knob flock9_rc 1 # a flock that answers 1 to a failure that is not a held lock
run_backup
expect_not_started "the singleton lock cannot be taken (flock exits 1)"
check "flock 1: says why" \
    "$(grep -c "Cannot lock $RUN_DIR/das-backup.lock (flock exit 1) — could not start" "$STATE/out")" "1"

# ---------------------------------------------------------------------------
echo "== 0: a skip — another backup holds the singleton lock"
# ---------------------------------------------------------------------------
fresh
(
    exec 7>"$RUN_DIR/das-backup.lock"
    flock 7
    : >"$STATE/holding"
    exec sleep 30 # the holder is this pid, so killing it releases the lock
) &
HOLDER_PID=$!
for _ in $(seq 1 200); do [[ -e "$STATE/holding" ]] && break; sleep 0.05; done
run_backup
kill "$HOLDER_PID" 2>/dev/null
wait "$HOLDER_PID" 2>/dev/null
HOLDER_PID=""
check "singleton held: exit status" "$RC" "0"
check "singleton held: says it skips" "$(grep -c 'skipping this invocation' "$STATE/out")" "1"
check "singleton held: btrdasd never called" "$(called btrdasd)" "no"
check "singleton held: nothing mounted" "$(called mount)" "no"

# ---------------------------------------------------------------------------
echo "== 130/143: stopped by a signal, as \`systemctl stop\` does (unchanged)"
# ---------------------------------------------------------------------------
# The whole process group gets the signal, as systemd's control-group kill
# does, while btrbk runs. `exec` makes the background job the run itself, so
# `wait` reports the run's status, not a subshell's death by the signal.
run_signalled() { # run_signalled <signal>
    run_cmd
    (
        set -m # the run gets its own process group
        { exec "${CMD[@]}"; } >"$STATE/out" 2>&1 &
        pid=$!
        for _ in $(seq 1 400); do [[ -e "$STATE/btrbk_started" ]] && break; sleep 0.05; done
        kill "-$1" -- "-$pid"
        wait "$pid"
    ) 2>/dev/null
    RC=$?
    tripwire
}
fresh
knob btrbk_blocks 1
run_signalled TERM
check "SIGTERM while btrbk runs: exit status" "$RC" "143"
show_tail 143
check "SIGTERM: the run is recorded as failed" "$(recorded_as)" "failure"
check "SIGTERM: nothing left mounted" "$(left_mounted)" "nothing"

fresh
knob btrbk_blocks 1
run_signalled INT
check "SIGINT while btrbk runs: exit status" "$RC" "130"
show_tail 130

# ---------------------------------------------------------------------------
echo "== every exit path of a run that does not complete, by the rule"
# ---------------------------------------------------------------------------
# abort_exit_status is what cleanup() exits with when main() did not complete.
body="$(sed -n '/^abort_exit_status() {/,/^}/p' "$SRC")"
if [[ -z "$body" ]]; then
    bad "abort_exit_status() not found in backup-run.sh"
else
    eval "$body"
    for armed in false true; do
        for rc in 0 1 2 32 127 130 143; do
            case "$rc" in
            130 | 143) want="$rc" ;;
            *) want="$([[ "$armed" == true ]] && echo 3 || echo 1)" ;;
            esac
            check "abort_exit_status $rc, lock held=$armed" "$(abort_exit_status "$rc" "$armed")" "$want"
        done
    done
fi

# ---------------------------------------------------------------------------
echo "== both packaged unit sources treat 3 as success, and only 3"
# ---------------------------------------------------------------------------
# The units `btrdasd setup` writes are pinned by the Rust tests in
# indexer/src/setup/templates.rs.
for unit in das-backup.service.in das-backup-full.service.in; do
    file="$REPO_ROOT/systemd/$unit"
    # Every SuccessExitStatus= line, with the section it sits in.
    got="$(awk '/^\[/ { section = $0 } /^SuccessExitStatus=/ { print section " " $0 }' "$file")"
    check "$unit: SuccessExitStatus" "$got" "[Service] SuccessExitStatus=3"
done

echo
echo "passed=$pass failed=$fail"
[[ $fail -eq 0 ]] || exit 1
echo "BACKUP EXIT SEMANTICS SUITE GREEN"
