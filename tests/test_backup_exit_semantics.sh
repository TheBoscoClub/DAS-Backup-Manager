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
#            a target's or a source's state. The units `btrdasd setup` writes
#            — the only backup units since bd DAS-Backup-Manager-7rf — list
#            SuccessExitStatus=3, so systemd and cachyos-sentinel see success
#            and never restart the run, while the journal still shows status=3.
#   1        it could not start: config unreadable, btrdasd missing, an
#            argument error, not root, the backup or maintenance lock unusable.
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
# With 3 counted as success, an abort must not be silent (bd
# DAS-Backup-Manager-2my): every run that aborts with 3 before its report
# sends ONE report through send_report's relay path, subject ABORTED — what
# aborted, why, what was backed up, which targets were seen, whether the run
# is in the history, where the log is — and records ONE failed history row
# (--counts-unknown, no --success, the reason in --errors). Neither can change
# the status. A run that could not start sends and records nothing; a dry run
# neither; a signal records the run but sends nothing. With REAL_BTRDASD set
# (ctest sets it to the binary CMake built) each abort's captured record-run
# vector is replayed through the real binary.
#
# The singleton lock (bd DAS-Backup-Manager-ismb): only "held by another run"
# is a skip (0); a lock file that cannot be opened, or a flock that fails for
# any other reason, is "could not start" (1), and says why.
#
# The snapshot counters (bd DAS-Backup-Manager-bzw) are decided before the
# run status and the report, so a counter failure reads FAILURES DETECTED in
# the report, the history and the exit status alike.
#
# Source volumes (bd DAS-Backup-Manager-8cf): a run unmounts only the source
# mount points it mounted itself, each once, on every way out — a stop while
# mount runs included. One already mounted when it starts — as fstab mounts
# /dasRaid0 and the /.btrfs-* top levels — is used as found and never
# unmounted, and verification still refuses one that holds the wrong
# filesystem, leaving it mounted. A probe that cannot tell is said, and the
# helper unmounted anyway. The mountpoint stub answers as util-linux does:
# 0 a mount point, 32 not one, 1 an error or a path that is not there.
#
# The disconnect claim (bd DAS-Backup-Manager-jug6): "DAS can be safely
# disconnected" only when every target is known released. A target the
# probe cannot tell about fails the gate — exit 3, NOT safe, and why — and a
# clean probe or an absent drive's removed mount point changes nothing.
#
# Every run's mail goes to a stub mailx, which keeps each one.
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
# The host's name is bash's own $HOSTNAME (bd DAS-Backup-Manager-arv1), so no
# `hostname` program is whitelisted: CI's container has none, and a run that
# reached for one is "command not found" here. Each run is given HOSTNAME=
# test-host.example.org, which bash keeps; one case leaves it out, to see bash
# set it itself (neither unit source sets Environment=).
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
# The processes the liveness helper's own cases start (bd DAS-Backup-Manager-7q8o),
# so a case that stopped half way leaves none behind. A case that stops one with
# stop_proc takes it off this list: finish() signals what is left, and a pid
# already collected may be another process's by then.
TEST_PROCS=()
finish() {
    local p
    [[ -n "$HOLDER_PID" ]] && kill "$HOLDER_PID" 2>/dev/null && wait "$HOLDER_PID" 2>/dev/null
    # A case that failed may have left the stub mailx's processes running.
    declare -F reap_mail_stubs >/dev/null && reap_mail_stubs
    for p in "${TEST_PROCS[@]:-}"; do
        [[ -n "$p" ]] && kill "$p" 2>/dev/null
    done
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
# The bound on one mail send, shortened so a stalled relay costs this suite
# seconds, not a minute a case.
MAIL_LINE='MAIL_TIMEOUT_SECS=60 MAIL_KILL_AFTER_SECS=10'
for line in "$LOCK_LINE" "$MAINT_LINE" "$ROOT_LINE" "$MAIL_LINE"; do
    n="$(grep -cxF -- "$line" "$SRC")"
    [[ "$n" == 1 ]] || harness_broken "expected exactly one line '$line' in $SRC, found $n"
done
sed -e "s|^LOCKFILE=\"/run/das-backup.lock\"\$|LOCKFILE=\"$RUN_DIR/das-backup.lock\"|" \
    -e "s|^MAINTENANCE_LOCKFILE=\"/run/das-maintenance.lock\"\$|MAINTENANCE_LOCKFILE=\"$RUN_DIR/das-maintenance.lock\"|" \
    -e 's|^    if \[\[ \$EUID -ne 0 \]\]; then$|    if [[ ${DAS_TEST_EUID:?} -ne 0 ]]; then|' \
    -e 's|^MAIL_TIMEOUT_SECS=60 MAIL_KILL_AFTER_SECS=10$|MAIL_TIMEOUT_SECS=2 MAIL_KILL_AFTER_SECS=1|' \
    "$SRC" >"$COPY"
changed="$(diff "$SRC" "$COPY" | grep -c '^>')"
[[ "$changed" == 4 ]] || harness_broken "the copy differs from $SRC in $changed lines, not 4"
# The guard that matters: nothing outside a comment may still name a real
# lock. If this ever fires, the copy is never run.
#
# The copy's own locks are under $RUN_DIR, a /tmp path that itself ends in
# .../run/das-*, so that prefix is taken out first: whatever "/run/das-" is
# left can only be a real path. And the copy is read whole, with no pipe. The
# guard this replaces, `grep -v '^#' | grep -q '/run/das-'`, matched the
# sandbox's own paths, and under pipefail it read a grep -v killed by SIGPIPE
# (grep -q quits at its first match; the copy is larger than a pipe) as "no
# match": it never fired unloaded, and under load it aborted correct runs at
# random (bd DAS-Backup-Manager-d1r round 3, item A).
# 0: the copy names a real /run/das-* path; 1: it does not; 2: unreadable.
copy_names_a_real_lock() { # copy_names_a_real_lock <copy> <the sandbox's run dir>
    local code
    code="$(grep -v '^[[:space:]]*#' "$1")" || return 2
    code="${code//"$2/das-"/}"
    [[ "$code" == *"/run/das-"* ]]
}
guard_rc=0
copy_names_a_real_lock "$COPY" "$RUN_DIR" || guard_rc=$?
case "$guard_rc" in
1) ;;
0) harness_broken "the copy still names a /run/das- path outside a comment" ;;
*) harness_broken "cannot read the copy to check it: $COPY" ;;
esac

# ---------------------------------------------------------------------------
# PATH: stubs, and a whitelist of harmless tools
# ---------------------------------------------------------------------------
mkdir -p "$BIN" "$SYSBIN"
for tool in awk basename cat cut date dirname flock grep head mkdir mktemp mv \
    rm rmdir sed setsid sleep sort tail timeout touch tr wc; do
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
"backup boot-plan")
    # The plan btrbk.conf gives: @ and @home on the nvme volume.
    printf '@\troot-\tnvme\n@home\thome\tnvme\n'
    ;;
"recovery-os status")
    printf 'RECOVERY OS\n  stub reading\n'
    exit "$(knob recovery_rc 0)"
    ;;
"backup record-run")
    # The run's log file stops being writable right here, at the record.
    if [[ -f "$S/knobs/break_log_at_record" ]]; then
        log="$(cat "$S/knobs/break_log_at_record")"
        rm -f "$log"
        mkdir "$log"
    fi
    printf '%s\n' "$@" >"$S/record_args"
    # The exact vector, NUL-separated, for the replay through the real binary.
    printf '%s\0' "$@" >"$S/record_vec"
    echo x >>"$S/record_calls"
    rc="$(knob record_rc 0)"
    [[ "$rc" == 0 ]] || echo "error: the history could not be written (stub)" >&2
    exit "$rc"
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
*" list latest "*)
    rc="$(knob list_rc 0)"
    if [[ "$rc" != 0 ]]; then
        echo "ERROR: Failed to fetch subvolume detail (stub)" >&2
        exit "$rc"
    fi
    if [[ " $* " == *" --format=raw "* ]]; then
        if [[ -f "$S/knobs/raw_unparsed" ]]; then
            # Fields this script's parser does not know (bd oi0, 06p).
            echo "format=\"latest\" snapshot_path='/v/.btrbk-snapshots/root-.20261004T0300' target_path='/t/nvme/root-.20261004T0300'"
        else
            echo "format=\"latest\" snapshot_subvolume='/v/.btrbk-snapshots/root-.20261004T0300' target_subvolume='/t/nvme/root-.20261004T0300'"
        fi
    else
        echo "SOURCE_SUBVOLUME SNAPSHOT_SUBVOLUME STATUS TARGET_SUBVOLUME"
        echo "/v/@ /v/.btrbk-snapshots/root-.20261004T0300 - /t/nvme/root-.20261004T0300"
    fi
    ;;
*" run "* | *" dryrun "*)
    mode=run
    [[ " $* " == *" dryrun "* ]] && mode=dryrun
    if [[ -f "$S/knobs/btrbk_blocks" ]]; then
        # Killing this sleep (a child of this pid) lets btrbk finish,
        # successfully, at once. In the foreground, so a signal to the group
        # reaches it: a background job of a non-interactive shell ignores
        # SIGINT, and would outlive the run holding its descriptors.
        echo "$$" >"$S/btrbk_stub.pid"
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
# Mounted, with something to say about it, as util-linux says it.
if listed mount_warns "$dst"; then
    echo "mount: $dst: WARNING: source write-protected, mounted read-only." >&2
fi
# A stop that lands while mount runs: block (until the test signals the run)
# before the mount is made, or after it is made.
if listed mount_blocks_before "$dst"; then
    : >"$S/mount_in"
    sleep 30
fi
uuid="${src#UUID=}"
listed wrong_fs_at "$dst" && uuid="a-different-filesystem"
printf '%s\t%s\t/\n' "$dst" "$uuid" >>"$S/mounted"
if listed mount_blocks_after "$dst"; then
    : >"$S/mount_in"
    sleep 30
fi
EOF

stub umount <<'EOF'
printf '%s\n' "$*" >>"$S/calls/umount"
dst="${*: -1}"
# A drive that went away can leave umount hanging: with the knob, wait (at
# most 10 s) until the test says go.
if [[ -f "$S/knobs/umount_waits" ]]; then
    i=0
    while [[ ! -e "$S/umount_release" ]] && ((i < 200)); do
        : >"$S/umount_waiting"
        sleep 0.05
        i=$((i + 1))
    done
fi
if listed umount_fails_once "$dst" && ! listed umount_failed "$dst"; then
    echo "$dst" >>"$S/knobs/umount_failed"
    echo "umount: $dst: target is busy (stub)." >&2
    exit 32
fi
if listed umount_fails_always "$dst"; then
    echo "umount: $dst: target is busy (stub)." >&2
    exit 32
fi
is_mounted "$dst" || { echo "umount: $dst: not mounted (stub)." >&2; exit 32; }
awk -F'\t' -v p="$dst" '$1 != p' "$S/mounted" >"$S/mounted.new"
mv "$S/mounted.new" "$S/mounted"
EOF

# As util-linux 2.42.4 answers (measured): 0 a mount point, 32 not one, 1 an
# error — and 1, "No such file or directory", for a path that does not exist
# (an absent target's mount point, which the run removes). A path listed in
# probe_fails_after_btrbk answers an error once btrbk has run, as a probe on a
# link that went bad mid-run might.
stub mountpoint <<'EOF'
p="${*: -1}"
if listed probe_fails_after_btrbk "$p" && grep -qE '(^| )(run|dryrun)$' "$S/calls/btrbk" 2>/dev/null; then
    echo "mountpoint: $p: Input/output error (stub)" >&2
    exit 1
fi
is_mounted "$p" && exit 0
if [[ ! -e "$p" ]]; then
    echo "mountpoint: $p: No such file or directory" >&2
    exit 1
fi
exit 32
EOF

stub findmnt <<'EOF'
cols="" target="" fstab=""
while [[ $# -gt 0 ]]; do
    case "$1" in
    -o) cols="$2"; shift ;;
    --target | --mountpoint) target="$2"; shift ;;
    --fstab) fstab=1 ;;
    esac
    shift
done
# fstab, as the knob declares it: the entry for <target>, or none (1).
if [[ -n "$fstab" ]]; then
    listed fstab_declares "$target" || exit 1
    echo "$target"
    exit 0
fi
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

# `systemctl show --value` of a timer: empty (exit 0) while the timer's own
# service runs (measured, bd hyvh); knob timer_next gives the value, knob
# timer_fails makes it exit 1.
stub systemctl <<'EOF'
if [[ -f "$S/knobs/timer_fails" ]]; then exit 1; fi
printf '%s\n' "$(knob timer_next '')"
EOF

# Keeps each mail: its arguments (the subject follows -s) and its body.
stub mailx <<'EOF'
n=$(($(cat "$S/mail_count" 2>/dev/null || echo 0) + 1))
echo "$n" >"$S/mail_count"
printf '%s\n' "$@" >"$S/mail.$n.args"
cat >"$S/mail.$n.body"
# A relay that took the connection and never finishes — one that keeps
# trickling bytes, which s-nail does not give up on (it gives up after about
# 45 s of silence) — with a child of mailx's own: mailx waits on it.
if [[ -f "$S/knobs/mail_stalls" ]]; then
    echo "$$" >"$S/mail_stall.pid"
    sleep 3600 &
    echo "$!" >"$S/mail_stall_child.pid"
    wait
fi
# The same, with a helper that leaves the process group: no kill of mailx
# reaches it, and whatever descriptors it inherited it keeps.
if [[ -f "$S/knobs/mail_stall_escapes" ]]; then
    echo "$$" >"$S/mail_stall.pid"
    setsid sleep 3600 </dev/null >/dev/null 2>&1 &
    echo "$!" >"$S/mail_escaped.pid"
    sleep 3600 &
    echo "$!" >"$S/mail_stall_child.pid"
    wait
fi
rc="$(knob mail_rc 0)"
[[ "$rc" == 0 ]] || echo "smtp-server: 127.0.0.1:25: Connection refused (stub)" >&2
exit "$rc"
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

# The two summaries the real pruner prints, a real run's and a dry run's (bd
# DAS-Backup-Manager-zwr). The stub used to print the first whatever it was
# asked, so a dry run passed here and failed on the host. This stub is only as
# faithful as this text: tests/test_boot_archive_cleanup.sh runs the pruner
# itself and reads its output back through run_archive_cleanup.
stub boot-archive-cleanup.sh <<'EOF'
if [[ " $* " == *" --dryrun "* ]]; then
    echo "[das-backup-22tb] Would delete 0, kept 0, errors 0"
else
    echo "[das-backup-22tb] Deleted 0, kept 0, errors 0"
fi
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
DAS_BOOT_ENABLED='$(cat "$STATE/knobs/boot_enabled" 2>/dev/null || echo false)'
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
DAS_EMAIL_ENABLED=true
DAS_EMAIL_SMTP_HOST='127.0.0.1'
DAS_EMAIL_SMTP_PORT=25
DAS_EMAIL_FROM='das-backup@example.test'
DAS_EMAIL_TO='operator@example.test'
EOF
}

# A new sandbox: nothing mounted, both targets present, every knob at its
# default (every stub succeeds).
fresh() {
    reap_mail_stubs
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
# What the run's environment says HOSTNAME is; empty leaves it out, and bash
# then sets it itself.
RUN_HOSTNAME=test-host.example.org
RC=""
# The command line of one run, into CMD: the copy, under `env -i`. SIGPIPE
# starts at its default, as it does in a terminal: a shell cannot trap a
# signal it inherited ignored, so a suite started with PIPE ignored would
# otherwise test nothing about it (systemd starts units with PIPE ignored).
run_cmd() {
    local -a host_env=()
    [[ -n "$RUN_HOSTNAME" ]] && host_env=(HOSTNAME="$RUN_HOSTNAME")
    CMD=(env -i --default-signal=PIPE
        PATH="$BIN:$SYSBIN" HOME="$WORK/home" LC_ALL=C TMPDIR="$WORK/tmp"
        DAS_TEST_STATE="$STATE" DAS_TEST_EUID="$RUN_EUID"
        BTRDASD_BIN="$RUN_BTRDASD" DAS_CONFIG="$WORK/etc/config.toml"
        BOOT_ARCHIVE_CLEANUP_BIN="$BIN/boot-archive-cleanup.sh"
        DAS_RECOVERY_OS_STATE="$WORK/lib/recovery-os.json"
        "${host_env[@]}"
        "$BASH" "$COPY" "$@")
}
tripwire() {
    if grep -q 'command not found' "$STATE/out"; then
        bad "the run reached a command that is neither stubbed nor whitelisted: $(grep -m1 'command not found' "$STATE/out")"
    fi
    # Under set -u an unset variable ends bash where it stands — in cleanup()
    # too, which would skip the unmount and the exit status it owes.
    if grep -q 'unbound variable' "$STATE/out"; then
        bad "the run met an unbound variable: $(grep -m1 'unbound variable' "$STATE/out")"
    fi
}
# run_backup [args...]: one run, to completion; its status in RC.
run_backup() {
    run_cmd "$@"
    "${CMD[@]}" >"$STATE/out" 2>&1
    RC=$?
    tripwire
}
# run_backup_within <deadline secs> [args...]: one run that might hang, under
# a deadline of the suite's own; its status in RC (124 if the deadline ended
# it), its wall time in seconds in ELAPSED.
run_backup_within() {
    local deadline="$1" start
    shift
    run_cmd "$@"
    start=$SECONDS
    timeout -k 5 "$deadline" "${CMD[@]}" >"$STATE/out" 2>&1
    RC=$?
    ELAPSED=$((SECONDS - start))
    tripwire
}
# "yes" when both of the run's locks can be taken now — no process the run
# left behind still holds either.
locks_free() {
    if "$REAL_FLOCK" -n "$RUN_DIR/das-backup.lock" true &&
        "$REAL_FLOCK" -n "$RUN_DIR/das-maintenance.lock" true; then
        echo yes
    else
        echo no
    fi
}
# pid_alive <pid>: succeeds when <pid> is a process that is still running.
# Never `kill -0`: it succeeds on a zombie, a process that has died and waits
# for its parent to collect it, and where PID 1 collects nothing (GitHub's job
# container runs `tail -f /dev/null` as PID 1) every orphan stays one. The
# bound's kill of a stalled mailx orphans its sleep child exactly so (CI run
# 37237268866). So /proc/<pid> must exist and its state must not be Z. The
# state is the first field after the LAST ')' of stat: comm sits inside the
# parentheses and may itself hold spaces and parentheses.
pid_alive() {
    local stat state
    # ASCII digits only, none leading. "" would read /proc/stat, and 0 is no
    # process: `kill -0 0` signals the caller's own group and always succeeds.
    # [[:digit:]], not [0-9] or [1-9]: under en_US.UTF-8 bash's regex ranges
    # also match digits of other scripts (bd DAS-Backup-Manager-1bsx).
    [[ "${1:-}" =~ ^[[:digit:]]+$ && "$1" != 0* ]] || return 1
    # No /proc entry: reaped, or gone since the caller noted it. The braces
    # silence bash's own "No such file"; on the assignment alone it leaks.
    { stat="$(<"/proc/$1/stat")"; } 2>/dev/null || return 1
    read -r state _ <<<"${stat##*)}"
    [[ "$state" != Z ]]
}
# "yes" when none of the processes the stub mailx recorded is still running,
# "no" when one is, and "never ran" when it recorded nothing: with no pid file
# there is no stall to have ended, and reading that as "gone" made the check
# pass vacuously (bd DAS-Backup-Manager-7q8o). Its siblings — the bound's
# elapsed time, the "did not finish" line — would fail too, but this one must
# not rest on them. The pid files are the run's own, in $STATE, unless a
# directory is named: the helper's own cases name one.
mail_stubs_gone() { # mail_stubs_gone [<directory holding the pid files>]
    local dir="${1:-$STATE}" f
    for f in "$dir/mail_stall.pid" "$dir/mail_stall_child.pid"; do
        if [[ ! -f "$f" ]]; then
            echo "never ran"
            return
        fi
    done
    for f in "$dir/mail_stall.pid" "$dir/mail_stall_child.pid"; do
        if pid_alive "$(cat "$f")"; then
            echo no
            return
        fi
    done
    echo yes
}
# Ends whatever the stub mailx left running, the escaped helper included.
reap_mail_stubs() {
    local f
    for f in "$STATE"/mail_*.pid; do
        [[ -f "$f" ]] && kill "$(cat "$f")" 2>/dev/null
    done
    return 0
}

called() { [[ -s "$STATE/calls/$1" ]] && echo yes || echo no; }
ran_btrbk() { grep -qE '(^| )(run|dryrun)$' "$STATE/calls/btrbk" 2>/dev/null && echo yes || echo no; }
left_mounted() {
    local m
    m="$(cut -f1 "$STATE/mounted" | tr '\n' ' ')"
    echo "${m:-nothing}"
}
# No pipe into an early-exiting reader in this suite (head, grep -q): under
# pipefail a producer killed by SIGPIPE turns a match into "no match".
report_status() { sed -n '/^  Status: /{s///p;q;}' "$WORK/lib/last-report.txt" 2>/dev/null; }
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

# The mails the stub mailx kept, one by one.
mails() { cat "$STATE/mail_count" 2>/dev/null || echo 0; }
mail_subject() { sed -n '/^-s$/{n;p;q;}' "$STATE/mail.$1.args" 2>/dev/null; }
mail_body() { cat "$STATE/mail.$1.body" 2>/dev/null; }
# The status in a subject: "[DAS Backup] <host> — <STATUS> — <date>".
mail_status() { mail_subject "$1" | awk -F ' — ' '{ print $2 }'; }
# ... and what comes before it: "[DAS Backup] <host>".
mail_subject_head() { mail_subject "$1" | awk -F ' — ' '{ print $1 }'; }
# The From of mail <n>: the argument after -r.
mail_from() { sed -n '/^-r$/{n;p;q;}' "$STATE/mail.$1.args" 2>/dev/null; }
# The value after "  <label>:" in mail <n>.
body_field() { sed -n "/^  $2: */{s///p;q;}" "$STATE/mail.$1.body" 2>/dev/null; }
record_calls() { if [[ -f "$STATE/record_calls" ]]; then wc -l <"$STATE/record_calls" | tr -d ' '; else echo 0; fi; }
vector_has() { grep -qxF -- "$1" "$STATE/record_args" 2>/dev/null && echo yes || echo no; }
# The value after <option> in the captured record-run vector.
vector_value() {
    local -a v=()
    local i
    [[ -f "$STATE/record_vec" ]] || return 0
    mapfile -d '' -t v <"$STATE/record_vec"
    for ((i = 0; i < ${#v[@]} - 1; i++)); do
        if [[ "${v[i]}" == "$1" ]]; then
            printf '%s\n' "${v[i + 1]}"
            return 0
        fi
    done
}

# The captured record-run vector through the real btrdasd, into a fresh
# database: it must parse, and the row it writes must say failed, with the
# counts unknown — the 6wt contract, per abort class. The row's errors are
# not shown by any btrdasd output: expect_aborted reads them in the vector,
# and indexer/tests/record_run_contract.rs reads the stored ones back with
# SQL. Without REAL_BTRDASD this is not run, and the summary says so.
NOT_RUN=0
replay() { # replay <name>
    if [[ -z "${REAL_BTRDASD:-}" ]]; then
        NOT_RUN=$((NOT_RUN + 1))
        return
    fi
    local db="$WORK/replay.db" out json i
    local -a v=()
    rm -f "$db" "$db-wal" "$db-shm"
    mapfile -d '' -t v <"$STATE/record_vec"
    for ((i = 0; i < ${#v[@]} - 1; i++)); do
        [[ "${v[i]}" == --db ]] && v[i + 1]="$db"
    done
    if ! out="$("$REAL_BTRDASD" "${v[@]}" 2>&1)"; then
        bad "$1: the real btrdasd refused the vector: $out"
        return
    fi
    json="$("$REAL_BTRDASD" --json backup report --db "$db" 2>&1)"
    check "$1: replayed through the real btrdasd: one run, failed, counts unknown" \
        "$(grep -oE '"(success|snaps_created|snaps_sent)": ?[a-z]+' <<<"$json" | tr -d ' ' | sort | tr '\n' ' ')" \
        '"snaps_created":null "snaps_sent":null "success":false '
}

# A run that reached its report: the exit status, the report's status line,
# the history row, the one mail, and nothing left mounted.
expect_completed() { # expect_completed <name> <status> <report status> <recorded as>
    local subject
    case "$3" in
    "ALL OPERATIONS SUCCESSFUL") subject="SUCCESS" ;;
    "COMPLETED WITH WARNINGS") subject="SUCCESS WITH WARNINGS" ;;
    *) subject="FAILURE" ;;
    esac
    check "$1: exit status" "$RC" "$2"
    show_tail "$2"
    check "$1: report status" "$(report_status)" "$3"
    check "$1: history row" "$(recorded_as)" "$4"
    check "$1: nothing left mounted" "$(left_mounted)" "nothing"
    check "$1: one mail" "$(mails)" "1"
    check "$1: its subject" "$(mail_status 1)" "$subject"
}
# A run that aborted once it held the maintenance lock: 3, btrbk never ran,
# cleanup() unmounted what the run had mounted — and the abort is not silent
# (bd DAS-Backup-Manager-2my): one ABORTED report saying what and why, and
# one failed history row with the reason in its errors.
expect_aborted() { # expect_aborted <name> <what the log says> <what aborted> <in the reason>
    check "$1: exit status" "$RC" "3"
    show_tail "3"
    check "$1: btrbk never ran" "$(ran_btrbk)" "no"
    check "$1: says why" "$(grep -qF -- "$2" "$STATE/out" && echo yes || echo no)" "yes"
    check "$1: cleanup() ran its recovery body" \
        "$(grep -c 'Cleaning up after abnormal termination' "$STATE/out")" "1"
    check "$1: nothing left mounted" "$(left_mounted)" "nothing"
    check "$1: one mail" "$(mails)" "1"
    check "$1: it says ABORTED" "$(mail_status 1)" "ABORTED"
    check "$1: what aborted" "$(body_field 1 'What aborted')" "$3"
    check "$1: why" "$(grep -qF -- "$4" "$STATE/mail.1.body" 2>/dev/null && echo yes || echo no)" "yes"
    check "$1: nothing was backed up" "$(body_field 1 'Backed up')" \
        "nothing — the run stopped before btrbk started"
    check "$1: the log" "$(body_field 1 'Log')" "$WORK/log/das-backup.log"
    check "$1: the history" "$(body_field 1 'History')" "recorded as failed"
    check "$1: written to the last report first" "$(report_status)" "ABORTED"
    check "$1: recorded once" "$(record_calls)" "1"
    check "$1: recorded as failed" "$(recorded_as)" "failure"
    check "$1: counts unknown" "$(vector_has --counts-unknown)" "yes"
    check "$1: the abort in the errors" "$(vector_value --errors | grep -c "^aborted: $3: ")" "1"
    replay "$1"
}
# A run that could not start: 1, and nothing was mounted or sent. Nor was
# anything unmounted: without the maintenance lock cleanup() must not touch
# the targets a scrub may hold (bd DAS-Backup-Manager-oeo). Nothing new is
# mailed or recorded either.
expect_not_started() { # expect_not_started <name>
    check "$1: exit status" "$RC" "1"
    show_tail "1"
    check "$1: nothing mounted" "$(called mount)" "no"
    check "$1: btrbk never ran" "$(ran_btrbk)" "no"
    check "$1: cleanup() skipped its recovery body" \
        "$(grep -c 'Cleaning up after abnormal termination' "$STATE/out")" "0"
    check "$1: nothing unmounted" "$(called umount)" "no"
    check "$1: no mail" "$(mails)" "0"
    check "$1: not recorded" "$(record_calls)" "0"
}

# ---------------------------------------------------------------------------
echo "== pid_alive and mail_stubs_gone: the liveness helper's own branches (bd 7q8o)"
# ---------------------------------------------------------------------------
# pid_alive replaced `kill -0`, which is true of a zombie. Three of its branches
# never ran on a developer's machine. The zombie one: on such a machine PID 1,
# or a subreaper, collects an orphan at once, so only a container whose PID 1
# collects nothing — GitHub's runs `tail -f /dev/null` — ever had a zombie to
# test. The state read after the LAST ')' of stat: no process this suite
# starts has a ')' in its name. And the pid guard. A later "simplification" to
# `kill -0`, to a look at /proc/<pid> alone, or to the first ')' would have
# stayed green here and turned CI red again. Each is a case below on real
# processes, with a control that shows the case is what it says: a zombie is
# shown to be one by the kernel (State: Z in /proc/<pid>/status, which
# pid_alive does not read) and by `kill -0` still succeeding on it.
liveness() { pid_alive "${1:-}" && echo alive || echo gone; }
# The kernel's own word for a process's state — R, S, D, T, Z — read from
# /proc/<pid>/status, or "absent".
proc_state() {
    local state
    state="$(sed -n 's/^State:[[:space:]]*\(.\).*/\1/p' "/proc/$1/status" 2>/dev/null)"
    echo "${state:-absent}"
}
# live <program> [args]: start it, remember it for finish(); its pid in LIVE_PID.
live() {
    "$@" >/dev/null 2>&1 &
    LIVE_PID=$!
    TEST_PROCS+=("$LIVE_PID")
}
# stop_proc <pid>: kill and collect a process started above, and take it off
# TEST_PROCS. finish() signals whatever is still on that list when the suite
# ends, and a pid already collected may by then be another process's: one run
# of this suite uses about 56,000 pids, so on a host at the kernel's default
# pid_max of 32768 the counter wraps within it (independent review, M2).
stop_proc() {
    local pid="$1" p
    local -a rest=()
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    for p in "${TEST_PROCS[@]:-}"; do
        [[ -n "$p" && "$p" != "$pid" ]] && rest+=("$p")
    done
    TEST_PROCS=("${rest[@]:-}")
}
# How often a pid is on that list.
procs_listed() { # procs_listed <pid>
    local p n=0
    for p in "${TEST_PROCS[@]:-}"; do
        [[ "$p" == "$1" ]] && n=$((n + 1))
    done
    echo "$n"
}
# wait_comm <pid> <name>: until the kernel names the process so — comm is set
# when it execs, a moment after the fork.
wait_comm() {
    local i
    for ((i = 0; i < 100; i++)); do
        [[ "$(cat "/proc/$1/comm" 2>/dev/null)" == "$2" ]] && return 0
        sleep 0.05
    done
    harness_broken "process $1 never got the name '$2' (it is '$(cat "/proc/$1/comm" 2>/dev/null)')"
}
# A real zombie: a child that has exited, whose parent never collects it. The
# parent starts the child in the background, notes its pid, then becomes
# `sleep`, which never calls wait, so the child stays a zombie on any machine,
# whoever PID 1 is, until its parent is killed. ZOMBIE_PARENT and ZOMBIE_CHILD.
make_zombie() { # make_zombie <child program> [args]
    local pidfile="$WORK/zombie.child" i
    rm -f "$pidfile"
    (
        "$@" >/dev/null 2>&1 &
        echo "$!" >"$pidfile"
        exec sleep 60
    ) &
    ZOMBIE_PARENT=$!
    TEST_PROCS+=("$ZOMBIE_PARENT")
    for ((i = 0; i < 100; i++)); do
        [[ -s "$pidfile" ]] && break
        sleep 0.05
    done
    ZOMBIE_CHILD="$(cat "$pidfile" 2>/dev/null)"
    [[ -n "$ZOMBIE_CHILD" ]] || harness_broken "the zombie's parent never noted its child"
    for ((i = 0; i < 100; i++)); do
        [[ "$(proc_state "$ZOMBIE_CHILD")" == Z ]] && return 0
        sleep 0.05
    done
    harness_broken "no zombie after 5 s: process $ZOMBIE_CHILD is '$(proc_state "$ZOMBIE_CHILD")' (is SIGCHLD ignored here?)"
}
# End the case: the parent is killed and collected; its orphan is then init's.
end_zombie() {
    stop_proc "$ZOMBIE_PARENT"
}

REAL_SLEEP="$(type -P sleep)" || harness_broken "no sleep on this system"
ODDCOMM="$WORK/oddcomm"
mkdir -p "$ODDCOMM"
# Two names with ')' in them. A parse that cuts stat at the FIRST ')' reads the
# state of the first as Z and of the second as S — each the wrong way round.
LIVE_ODD="a) Z (b"
ZOMBIE_ODD="z) S (z"
ln -s "$REAL_SLEEP" "$ODDCOMM/$LIVE_ODD"
ln -s "$REAL_SLEEP" "$ODDCOMM/$ZOMBIE_ODD"

# A live process is alive; killed and collected, it is gone.
live sleep 60
check "pid_alive, a live process: alive" "$(liveness "$LIVE_PID")" "alive"
check "cleanup list, a process still running: finish() would signal it" "$(procs_listed "$LIVE_PID")" "1"
stop_proc "$LIVE_PID"
check "pid_alive, the same process killed and collected: gone" "$(liveness "$LIVE_PID")" "gone"
check "cleanup list, the same process, collected: no longer on it, so finish() cannot signal a reused pid" \
    "$(procs_listed "$LIVE_PID")" "0"
DEAD_PID="$LIVE_PID"

# A zombie is gone, though kill -0 says otherwise; its parent, which has not
# collected it, is not.
make_zombie sleep 0
check "zombie control: the kernel says it is a zombie" "$(proc_state "$ZOMBIE_CHILD")" "Z"
check "zombie control: kill -0 still succeeds on it, which is why that was the wrong test" \
    "$(kill -0 "$ZOMBIE_CHILD" 2>/dev/null && echo yes || echo no)" "yes"
check "pid_alive, a zombie: gone" "$(liveness "$ZOMBIE_CHILD")" "gone"
check "pid_alive, the zombie's parent, which never collects it: alive" "$(liveness "$ZOMBIE_PARENT")" "alive"
end_zombie
check "zombie case ended: its parent and the orphan are both gone" \
    "$(liveness "$ZOMBIE_PARENT") $(liveness "$ZOMBIE_CHILD")" "gone gone"

# A ')' in the name: the state is read after the LAST one.
live "$ODDCOMM/$LIVE_ODD" 60
ODD_PID="$LIVE_PID"
wait_comm "$ODD_PID" "$LIVE_ODD"
check "odd comm control: the kernel's name for the live process holds ') Z ('" \
    "$(cat "/proc/$ODD_PID/comm")" "$LIVE_ODD"
check "odd comm control: and it is running" "$(proc_state "$ODD_PID")" "S"
check "pid_alive, a live process named '$LIVE_ODD': alive" "$(liveness "$ODD_PID")" "alive"
stop_proc "$ODD_PID"
check "pid_alive, the same process killed and collected: gone" "$(liveness "$ODD_PID")" "gone"

make_zombie "$ODDCOMM/$ZOMBIE_ODD" 0
check "odd comm control: the zombie is named '$ZOMBIE_ODD'" "$(cat "/proc/$ZOMBIE_CHILD/comm")" "$ZOMBIE_ODD"
check "odd comm control: and the kernel says it is a zombie" "$(proc_state "$ZOMBIE_CHILD")" "Z"
check "pid_alive, a zombie named '$ZOMBIE_ODD': gone" "$(liveness "$ZOMBIE_CHILD")" "gone"
end_zombie

# What is no pid is gone. "" and "self" are the ones that would read a file:
# /proc//stat is /proc/stat, and /proc/self/stat is the reader's own.
for bad in "" 0 007 abc -1 "1 2" self thread-self 4194304999; do
    check "pid_alive '$bad': gone" "$(liveness "$bad")" "gone"
done
check "pid_alive, no argument: gone" "$(pid_alive && echo alive || echo gone)" "gone"

# mail_stubs_gone: "never ran" when the stub recorded no pid, not "gone".
live sleep 60
LIVE_FOR_MAIL="$LIVE_PID"
mail_state() { # mail_state [first pid-file's content [second's]]: what mail_stubs_gone says
    local dir="$WORK/mail-state"
    rm -rf "$dir"
    mkdir -p "$dir"
    [[ $# -ge 1 ]] && printf '%s\n' "$1" >"$dir/mail_stall.pid"
    [[ $# -ge 2 ]] && printf '%s\n' "$2" >"$dir/mail_stall_child.pid"
    mail_stubs_gone "$dir"
}
check "mail_stubs_gone, no pid recorded at all: never ran" "$(mail_state)" "never ran"
check "mail_stubs_gone, only the first pid recorded: never ran" "$(mail_state "$DEAD_PID")" "never ran"
check "mail_stubs_gone, both recorded, both gone: yes" "$(mail_state "$DEAD_PID" "$DEAD_PID")" "yes"
check "mail_stubs_gone, both recorded, the first alive: no" "$(mail_state "$LIVE_FOR_MAIL" "$DEAD_PID")" "no"
check "mail_stubs_gone, both recorded, the second alive: no" "$(mail_state "$DEAD_PID" "$LIVE_FOR_MAIL")" "no"
stop_proc "$LIVE_FOR_MAIL"
# Forgetting must be exact: with two running, stopping one leaves the other
# listed, so a case that stops half way still leaves nothing behind.
live sleep 60
FIRST_LISTED="$LIVE_PID"
live sleep 60
SECOND_LISTED="$LIVE_PID"
stop_proc "$FIRST_LISTED"
check "cleanup list, stopping one of two running processes: it goes, the other stays" \
    "$(procs_listed "$FIRST_LISTED") $(procs_listed "$SECOND_LISTED")" "0 1"
stop_proc "$SECOND_LISTED"
check "cleanup list, every process this section started has been collected: nothing is left for finish() to signal" \
    "$(printf '%s' "${TEST_PROCS[*]:-}" | tr -d ' ')" ""

# ---------------------------------------------------------------------------
echo "== 0: the run executed and nothing failed"
# ---------------------------------------------------------------------------
fresh
run_backup
expect_completed "clean run" 0 "ALL OPERATIONS SUCCESSFUL" success
check "clean run: btrbk ran" "$(ran_btrbk)" "yes"
check "clean run: the snapshot counts row" \
    "$(grep -c '^  Snapshot counts       OK  (1 created, 1 sent)$' "$WORK/lib/last-report.txt")" "1"
check "clean run: counted, not unknown" "$(vector_has --counts-unknown)" "no"
check "clean run: a silent mount logs no mount warning" \
    "$(grep -c 'WARN.*mount said' "$STATE/out")" "0"

# The report's "Next scheduled:" line is never blank (bd hyvh): systemd prints
# an empty value, exit 0, while the timer's own service runs.
next_line() { sed -n 's/^  Next scheduled: //p' "$WORK/lib/last-report.txt"; }
fresh
run_backup
check "next scheduled, empty value (the timer's own run is going): unknown" "$(next_line)" "unknown"
fresh
knob timer_next "Wed 2026-10-07 03:05:47 CDT"
run_backup
check "next scheduled, a date: the zone is dropped" "$(next_line)" "Wed 2026-10-07 03:05:47"
fresh
knob timer_next "n/a"
run_backup
check "next scheduled, n/a: unknown" "$(next_line)" "unknown"
fresh
knob timer_fails 1
run_backup
check "next scheduled, systemctl fails: unknown" "$(next_line)" "unknown"

# A source that mounts with something to say — util-linux's "source
# write-protected, mounted read-only" — still says it: mount_sources()
# captures mount's output to name the device if it fails (round 3, M4), and
# that capture dropped it on success, where it used to reach the journal
# (round 4, N1). It is logged as a warning, in the journal and the log file.
fresh
knob mount_warns "$SOURCE_MNT"
run_backup
expect_completed "a source mounts with a warning" 0 "ALL OPERATIONS SUCCESSFUL" success
check "a source mounts with a warning: in the journal, naming the source" \
    "$(grep -c "WARN.*mount said, mounting nvme (UUID=source-uuid) at $SOURCE_MNT: mount: $SOURCE_MNT: WARNING: source write-protected, mounted read-only\." "$STATE/out")" "1"
check "a source mounts with a warning: in the log file" \
    "$(grep -c 'WARN.*mount said, mounting nvme .*source write-protected, mounted read-only' "$WORK/log/das-backup.log")" "1"

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
check "clean dry run: no mail" "$(mails)" "0"
check "clean dry run: not recorded" "$(record_calls)" "0"

# ---------------------------------------------------------------------------
echo "== the host's name is bash's own \$HOSTNAME, never the hostname program"
# ---------------------------------------------------------------------------
# On a host without the program — CI's container is one, and no packaging
# declares it — a run sent these with the name left blank: an empty "Host:", a
# subject of "[DAS Backup]  — SUCCESS", a From of "DAS Backup ()", and "command
# not found" in the journal (bd DAS-Backup-Manager-arv1). The whitelist has no
# `hostname` now, so a run that reaches for it fails tripwire(); these cases
# pin the value: the report's Host line and the subject carry HOSTNAME as it
# is, the From name its short form, everything before the first dot.
fresh
run_backup
check "host name, clean run: HOSTNAME was in the run's environment" \
    "$(printf '%s\n' "${CMD[@]}" | grep -c '^HOSTNAME=test-host.example.org$')" "1"
check "host name, clean run: exit status" "$RC" "0"
show_tail 0
check "host name, clean run: the report's Host line" "$(body_field 1 Host)" "test-host.example.org"
check "host name, clean run: the subject" "$(mail_subject_head 1)" "[DAS Backup] test-host.example.org"
check "host name, clean run: the From name is the short name" \
    "$(mail_from 1)" "DAS Backup (test-host) <das-backup@example.test>"

# The ABORTED report is built by a function of its own, and sent the same way.
fresh
knob wrong_fs_at "$PRIMARY_MNT"
run_backup
check "host name, ABORTED report: exit status" "$RC" "3"
show_tail 3
check "host name, ABORTED report: it is the ABORTED mail" "$(mail_status 1)" "ABORTED"
check "host name, ABORTED report: its Host line" "$(body_field 1 Host)" "test-host.example.org"
check "host name, ABORTED report: the subject" "$(mail_subject_head 1)" "[DAS Backup] test-host.example.org"
check "host name, ABORTED report: the From name" \
    "$(mail_from 1)" "DAS Backup (test-host) <das-backup@example.test>"

# Left out of the environment (neither unit source sets Environment=),
# HOSTNAME is set by bash itself from gethostname(): the name the kernel
# reports — read here from /proc, not from a program — and set under `set -u`,
# or tripwire() would fail the run on an unbound variable.
kernel_host=""
read -r kernel_host </proc/sys/kernel/hostname || harness_broken "cannot read the kernel's host name"
[[ -n "$kernel_host" ]] || harness_broken "the kernel's host name is empty"
RUN_HOSTNAME=""
fresh
run_backup
RUN_HOSTNAME=test-host.example.org
check "host name set by bash: HOSTNAME was left out of the run's environment" \
    "$(printf '%s\n' "${CMD[@]}" | grep -c '^HOSTNAME=')" "0"
check "host name set by bash: exit status" "$RC" "0"
show_tail 0
check "host name set by bash: the Host line is the kernel's" "$(body_field 1 Host)" "$kernel_host"
check "host name set by bash: the subject" "$(mail_subject_head 1)" "[DAS Backup] $kernel_host"
check "host name set by bash: the From name is the name up to its first dot" \
    "$(mail_from 1)" "DAS Backup ($(cut -d. -f1 <<<"$kernel_host")) <das-backup@example.test>"

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

# bd DAS-Backup-Manager-bzw: the snapshot counters are decided before the run
# status and the report, so a counter failure reads FAILURES DETECTED in the
# report, the history and the exit status alike — and the report says which.
fresh
knob list_rc 1 # btrbk ran, but `btrbk list latest` failed
run_backup
expect_completed "the snapshot counts unknown (btrbk list latest failed)" 3 "FAILURES DETECTED" failure
check "counts unknown: the report names it" \
    "$(grep -c '^  Snapshot counts       FAIL  (btrbk list latest failed; counts unknown)$' "$WORK/lib/last-report.txt")" "1"
check "counts unknown: recorded unknown" "$(vector_has --counts-unknown)" "yes"
check "counts unknown: btrbk itself succeeded" "$(grep -c 'btrbk completed' "$STATE/out")" "1"

fresh
knob raw_unparsed 1 # btrbk's raw listing holds no field the parser knows
run_backup
expect_completed "the snapshot counts unparsed" 3 "FAILURES DETECTED" failure
check "counts unparsed: the report names it" \
    "$(grep -c '^  Snapshot counts       FAIL  (raw output present but no fields parsed; counts unknown)$' "$WORK/lib/last-report.txt")" "1"
check "counts unparsed: warned once" \
    "$(grep -c 'no snapshot_subvolume/target_subvolume fields parsed' "$STATE/out")" "1"

# The history row is written after the report, and a record that fails is a
# FAIL (bd 6wt): the report is sent again, and the run exits 3.
fresh
knob record_rc 2
run_backup
check "a FAIL without btrbk failing (history record): exit status" "$RC" "3"
show_tail 3
check "history record failed: the report sent again says so" "$(report_status)" "FAILURES DETECTED"
check "history record failed: two mails" "$(mails)" "2"
check "history record failed: the second says FAILURE" "$(mail_status 2)" "FAILURE"

fresh
knob btrbk_dryrun_rc 10
run_backup --dryrun
check "dry run, btrbk dryrun exits 10: exit status" "$RC" "3"
show_tail 3
check "dry run, btrbk dryrun exits 10: no mail" "$(mails)" "0"

# ---------------------------------------------------------------------------
echo "== 3: the run began its work and aborted on a target's or a source's state"
# ---------------------------------------------------------------------------
fresh
knob wrong_fs_at "$PRIMARY_MNT"
run_backup
expect_aborted "verify_targets_before_btrbk (wrong filesystem on a target)" \
    "ABORTING — refusing to invoke btrbk" "target verification" \
    "primary-22tb: $PRIMARY_MNT has fs UUID 'a-different-filesystem', expected 'primary-uuid'"

fresh
knob mount_fails "$RECOVERY_MNT"
run_backup
expect_aborted "a target that fails to mount" "is NOT a mountpoint (mount failed silently in mount_targets)" \
    "target verification" "system-recovery-A-2tb (mirror): expected mounted but $RECOVERY_MNT is NOT a mountpoint"

fresh
printf '%s\n' primary-uuid >"$STATE/knobs/present_uuids"
mkdir -p "$RECOVERY_MNT"
echo "written while the drive was away" >"$RECOVERY_MNT/stray-file"
run_backup
expect_aborted "the bare-mountpoint guard (absent target, non-empty directory)" \
    "ABORTING: target system-recovery-A-2tb is unavailable but $RECOVERY_MNT is non-empty" \
    "the bare-mountpoint guard" "system-recovery-A-2tb is unavailable, but $RECOVERY_MNT is not empty"
check "bare-mountpoint guard: the targets seen" "$(body_field 1 'Targets seen')" "primary-22tb"
check "bare-mountpoint guard: the targets not seen" "$(body_field 1 'Targets not seen')" "system-recovery-A-2tb"

fresh
printf '%s\n' primary-uuid >"$STATE/knobs/present_uuids"
mkdir -p "$RECOVERY_MNT"
printf '%s\trecovery-a-uuid\t/\n' "$RECOVERY_MNT" >>"$STATE/mounted"
knob umount_fails_once "$RECOVERY_MNT"
run_backup
expect_aborted "an absent target still mounted that will not unmount" \
    "refusing to proceed — $RECOVERY_MNT is mounted but target is marked unavailable" \
    "mount point preparation" "system-recovery-A-2tb is unavailable, but $RECOVERY_MNT is mounted and will not unmount"

# The DAS powered off: before bd 2my this was silent every night.
fresh
printf '%s\n' recovery-a-uuid >"$STATE/knobs/present_uuids"
run_backup
expect_aborted "no primary target available" "No primary backup target is available — aborting" \
    "DAS detection" "no primary backup target is available"
check "no primary target: the targets seen" "$(body_field 1 'Targets seen')" "system-recovery-A-2tb"
check "no primary target: the targets not seen" "$(body_field 1 'Targets not seen')" "primary-22tb"

fresh
knob mount_fails "$SOURCE_MNT"
run_backup
# mount_sources() aborts with 3 itself, naming the source, its device and
# mount's own message (round 3, M4). It used to leave the failure to set -e,
# and the report and the history then carried the unexpanded command —
# `mount -t btrfs -o subvolid=5 "$dev" "$mnt"` — which names neither.
expect_aborted "a source that fails to mount" "wrong fs type, bad option" "source mount" \
    "nvme: UUID=source-uuid at $SOURCE_MNT: mount exited 32: mount: $SOURCE_MNT: wrong fs type, bad option, bad superblock (stub)"
check "a source that fails to mount: no unexpanded text in the report" \
    "$(grep -c '"\$' "$STATE/mail.1.body")" "0"
check "a source that fails to mount: nor in the history" "$(vector_value --errors | grep -c '"\$')" "0"

fresh
knob wrong_fs_at "$SOURCE_MNT"
run_backup
expect_aborted "verify_sources_before_write (wrong filesystem on a source)" \
    "ABORTING — refusing to write to source volumes" "source verification" \
    "nvme: $SOURCE_MNT has fs UUID 'a-different-filesystem', expected 'source-uuid'"

# The log file stops being writable mid-run (the root filesystem full, say):
# the next log line aborts the run under set -e, and cleanup()'s own log
# lines fail too. They must not end the trap early with their status (1) —
# the run still exits 3, still unmounts what it mounted, and still reports.
fresh
knob break_log "$WORK/log/das-backup.log"
run_backup
# A set -e failure has no guard to name it: the reason is its status and the
# call chain it failed in — here log() under one of the log_* helpers — not
# the unexpanded command text (round 3, M4).
expect_aborted "the log unwritable mid-run" "Is a directory" "a command that failed" "exit status 1 in log < log_"
check "the log unwritable mid-run: the chain, innermost first, ends in main()" \
    "$(grep -cE '^  Why: +exit status 1 in log < log_[a-z]+ < [a-z_]+ < main$' "$STATE/mail.1.body")" "1"
check "the log unwritable mid-run: main() once — bash's own top-level entry left out" \
    "$(grep -c 'main < main' "$STATE/mail.1.body")" "0"
check "the log unwritable mid-run: no unexpanded text in the report" \
    "$(grep -c '"\$' "$STATE/mail.1.body")" "0"

# Two findings at once: both in the report — one beside "Why:", the other on
# a line of its own below it — and both on the one `aborted:` line in the
# history's errors.
fresh
knob wrong_fs_at "$PRIMARY_MNT"
knob mount_fails "$RECOVERY_MNT"
run_backup
check "two violations: exit status" "$RC" "3"
show_tail 3
check "two violations: one beside Why" \
    "$(mail_body 1 | grep -cE '^  Why:               (primary-22tb|system-recovery-A-2tb)')" "1"
check "two violations: the other on its own line below" \
    "$(mail_body 1 | grep -cE '^ {21}(primary-22tb|system-recovery-A-2tb)')" "1"
check "two violations: one aborted line in the history, both in it" \
    "$(vector_value --errors | grep -E '^aborted: target verification: ' | grep -c 'primary-22tb: .*; .*system-recovery-A-2tb\|system-recovery-A-2tb.*; .*primary-22tb: ')" "1"

# --- the abort report and the history row are best effort: neither may change
# --- the status or hide the abort (bd 2my).
fresh
knob wrong_fs_at "$PRIMARY_MNT"
knob mail_rc 1 # the relay is down
run_backup
check "abort, relay down: exit status" "$RC" "3"
show_tail 3
check "abort, relay down: the report was still written first" "$(report_status)" "ABORTED"
check "abort, relay down: said so" "$(grep -c 'The report saying this run aborted was not emailed' "$STATE/out")" "1"
check "abort, relay down: says the file has it" \
    "$(grep -c "was not emailed — it is in $WORK/lib/last-report.txt" "$STATE/out")" "1"
check "abort, relay down: still recorded as failed" "$(recorded_as)" "failure"
check "abort, relay down: nothing left mounted" "$(left_mounted)" "nothing"

# A last report that cannot be written (a full disk, a directory where the
# file goes) is said, never claimed: no "Report saved", and each line saying
# the report was not emailed says where it is instead — the journal, which
# every report is echoed to before it is saved (round 3, M3).
fresh
mkdir -p "$WORK/lib/last-report.txt"
knob wrong_fs_at "$PRIMARY_MNT"
run_backup
check "abort, last report unwritable: exit status" "$RC" "3"
show_tail 3
check "abort, last report unwritable: never says it was saved" "$(grep -c 'Report saved to' "$STATE/out")" "0"
check "abort, last report unwritable: says it could not be saved" \
    "$(grep -c "Could not save the report to $WORK/lib/last-report.txt" "$STATE/out")" "1"
check "abort, last report unwritable: still emailed" "$(mails) $(mail_status 1)" "1 ABORTED"
check "abort, last report unwritable: still recorded as failed" "$(recorded_as)" "failure"

fresh
mkdir -p "$WORK/lib/last-report.txt"
knob wrong_fs_at "$PRIMARY_MNT"
knob mail_rc 1
run_backup
check "abort, last report unwritable, relay down: exit status" "$RC" "3"
show_tail 3
check "abort, last report unwritable, relay down: no claim the file has it" \
    "$(grep -c "it is in $WORK/lib/last-report.txt" "$STATE/out")" "0"
check "abort, last report unwritable, relay down: says the journal has it" \
    "$(grep -c 'was not emailed — it could not be saved' "$STATE/out")" "1"
check "abort, last report unwritable, relay down: and the journal has it" \
    "$(grep -c '^  Status: ABORTED$' "$STATE/out")" "1"

fresh
mkdir -p "$WORK/lib/last-report.txt"
knob mail_rc 1
run_backup
check "clean run, last report unwritable, relay down: exit status (email FAILED)" "$RC" "3"
show_tail 3
check "clean run, last report unwritable, relay down: never says it was saved" \
    "$(grep -c "Report saved to\|it is in $WORK/lib/last-report.txt" "$STATE/out")" "0"
check "clean run, last report unwritable, relay down: the history says where it is" \
    "$(vector_value --errors | grep -c '^email: delivery failed; it could not be saved')" "1"
check "clean run, last report unwritable, relay down: the journal has it" \
    "$(grep -c '^  DAS Backup Report — ' "$STATE/out")" "1"

# Email disabled, and the last report cannot be saved either: the report went
# nowhere that lasts — the journal has the only copy. Under the operator's
# rule that is something that failed (3), not a run that went well: it used
# to exit 0 with a success row (round 4, N4). The row says why, as a report
# failure, not an email one.
email_off() { sed -i 's/^DAS_EMAIL_ENABLED=true$/DAS_EMAIL_ENABLED=false/' "$STATE/env"; }
fresh
email_off
mkdir -p "$WORK/lib/last-report.txt"
run_backup
check "email disabled, last report unwritable: exit status" "$RC" "3"
show_tail 3
check "email disabled, last report unwritable: recorded as failed" "$(recorded_as)" "failure"
check "email disabled, last report unwritable: the history says why" \
    "$(vector_value --errors | grep -cxF "report: not saved to $WORK/lib/last-report.txt, and email is disabled: the journal has the only copy")" "1"
check "email disabled, last report unwritable: not an email failure" \
    "$(vector_value --errors | grep -c '^email:')" "0"
check "email disabled, last report unwritable: no mail" "$(mails)" "0"
check "email disabled, last report unwritable: the journal has it" \
    "$(grep -c '^  DAS Backup Report — ' "$STATE/out")" "1"
check "email disabled, last report unwritable: says the journal has the only copy" \
    "$(grep -c 'could not be saved: it is in the journal only' "$STATE/out")" "1"

# ... and with the file writable, email disabled is a run that went well.
fresh
email_off
run_backup
check "email disabled, last report saved: exit status" "$RC" "0"
show_tail 0
check "email disabled, last report saved: report status" "$(report_status)" "ALL OPERATIONS SUCCESSFUL"
check "email disabled, last report saved: recorded as a success" "$(recorded_as)" "success"
check "email disabled, last report saved: nothing left mounted" "$(left_mounted)" "nothing"
check "email disabled, last report saved: no mail" "$(mails)" "0"
check "email disabled, last report saved: says where it is" \
    "$(grep -c "Email reporting disabled in config — not emailed; it is in $WORK/lib/last-report.txt" "$STATE/out")" "1"
check "email disabled, last report saved: no report failure" \
    "$(vector_value --errors | grep -c '^report:')" "0"

fresh
knob wrong_fs_at "$PRIMARY_MNT"
knob record_rc 2 # the history cannot be written
run_backup
check "abort, history unwritable: exit status" "$RC" "3"
show_tail 3
check "abort, history unwritable: one mail, ABORTED" "$(mails) $(mail_status 1)" "1 ABORTED"
check "abort, history unwritable: the report says the run is missing" \
    "$(body_field 1 'History')" "NOT recorded — recording it failed: error: the history could not be written (stub)"
check "abort, history unwritable: nothing left mounted" "$(left_mounted)" "nothing"

fresh
knob wrong_fs_at "$PRIMARY_MNT"
knob mail_rc 1
knob record_rc 2
run_backup
check "abort, relay and history both down: exit status" "$RC" "3"
show_tail 3
check "abort, relay and history both down: the report is on disk" "$(report_status)" "ABORTED"
check "abort, relay and history both down: nothing left mounted" "$(left_mounted)" "nothing"

# An abort AFTER main() sent its report and recorded the run — the log turns
# unwritable at the record, and the next log line ends the run under set -e:
# it exits 3, but sends no second report and records no second row.
fresh
knob break_log_at_record "$WORK/log/das-backup.log"
run_backup
check "abort after the report: exit status" "$RC" "3"
show_tail 3
check "abort after the report: the one report only" "$(mails) $(mail_status 1)" "1 SUCCESS"
check "abort after the report: recorded once" "$(record_calls)" "1"
check "abort after the report: nothing left mounted" "$(left_mounted)" "nothing"

# The report and the record come before the unmount, which can hang on a
# drive that went away: while umount hangs, both must already be out.
fresh
knob wrong_fs_at "$PRIMARY_MNT"
knob umount_waits 1
run_cmd
"${CMD[@]}" >"$STATE/out" 2>&1 &
run_pid=$!
for _ in $(seq 1 200); do [[ -e "$STATE/umount_waiting" ]] && break; sleep 0.05; done
check "unmount hanging: the run is in the unmount" "$([[ -e "$STATE/umount_waiting" ]] && echo yes || echo no)" "yes"
check "unmount hanging: the ABORTED report is already out" "$(mails) $(mail_status 1)" "1 ABORTED"
check "unmount hanging: the run is already recorded" "$(record_calls)" "1"
: >"$STATE/umount_release"
wait "$run_pid"
RC=$?
tripwire
check "unmount hanging: exit status once it returns" "$RC" "3"
show_tail 3

# A dry run sends and records nothing, aborted or not.
fresh
knob wrong_fs_at "$PRIMARY_MNT"
run_backup --dryrun
check "dry run aborted: exit status" "$RC" "3"
show_tail 3
check "dry run aborted: no mail" "$(mails)" "0"
check "dry run aborted: not recorded" "$(record_calls)" "0"

# A relay that takes the connection and never finishes costs the report, not
# the run (round 3, M1). s-nail gives up after about 45 s of silence, but
# not on a relay that keeps trickling bytes, so an unbounded mailx kept the
# DAS mounted and both locks held for as long as such a relay trickled — a
# waiting scrub behind it, and the alert the very thing stuck (round 4, N2,
# corrected the "no read timeout" this said).
# Every send is bounded: in this copy TERM after 2 s, KILL 1 s later. The
# suite's own deadline (30 s) only ends a run that hangs regardless.
fresh
knob mail_stalls 1
knob wrong_fs_at "$PRIMARY_MNT"
run_backup_within 30
check "abort, relay stalls: exit status" "$RC" "3"
show_tail 3
check "abort, relay stalls: over within the bound" "$((ELAPSED <= 15))" "1"
check "abort, relay stalls: the send was given its time" "$((ELAPSED >= 2))" "1"
check "abort, relay stalls: says why it was not emailed" \
    "$(grep -c 'did not finish within 2 s' "$STATE/out")" "1"
check "abort, relay stalls: still recorded as failed" "$(recorded_as)" "failure"
check "abort, relay stalls: nothing left mounted" "$(left_mounted)" "nothing"
check "abort, relay stalls: both locks free" "$(locks_free)" "yes"
check "abort, relay stalls: the stalled mailx is gone" "$(mail_stubs_gone)" "yes"

fresh
knob mail_stalls 1
run_backup_within 30
check "clean run, relay stalls: exit status (report saved, email only a WARN — dlpr)" "$RC" "0"
show_tail 3
check "clean run, relay stalls: over within the bound" "$((ELAPSED <= 15))" "1"
check "clean run, relay stalls: says why it was not emailed" \
    "$(grep -c 'did not finish within 2 s' "$STATE/out")" "1"
check "clean run, relay stalls: recorded as a success (report saved — dlpr)" "$(recorded_as)" "success"
check "clean run, relay stalls: the failed delivery is still logged" "$(grep -c "Email delivery failed" "$STATE/out")" "1"
check "clean run, relay stalls: nothing left mounted" "$(left_mounted)" "nothing"
check "clean run, relay stalls: both locks free" "$(locks_free)" "yes"
check "clean run, relay stalls: the stalled mailx is gone" "$(mail_stubs_gone)" "yes"

# A mail helper that leaves mailx's process group outlives the bound's kill;
# it must not have inherited the run's locks (fds 8 and 9).
fresh
knob mail_stall_escapes 1
run_backup_within 30
check "clean run, mail helper escapes: exit status (report saved — dlpr)" "$RC" "0"
show_tail 3
check "clean run, mail helper escapes: over within the bound" "$((ELAPSED <= 15))" "1"
check "clean run, mail helper escapes: the helper is still alive" \
    "$(pid_alive "$(cat "$STATE/mail_escaped.pid" 2>/dev/null)" && echo yes || echo no)" "yes"
check "clean run, mail helper escapes: both locks free all the same" "$(locks_free)" "yes"
reap_mail_stubs

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
check "singleton held: no mail" "$(mails)" "0"

# ---------------------------------------------------------------------------
echo "== 129/130/138/141/142/143: stopped by a signal, as \`systemctl stop\` does"
# ---------------------------------------------------------------------------
# The whole process group gets the signal, as systemd's control-group kill
# does, while btrbk runs. `exec` makes the background job the run itself, so
# `wait` reports the run's status, not a subshell's death by the signal.
run_signalled() { # run_signalled <signal> [args...]
    run_signalled_when btrbk_started "$@"
}
# run_signalled_when <state file> <signal> [args...]: the same, with the
# signal sent once the stub that blocks has written <state file> —
# btrbk_started (btrbk runs), mount_in (a source's mount runs).
run_signalled_when() {
    local flag="$1" sig="$2"
    shift 2
    run_cmd "$@"
    (
        set -m # the run gets its own process group
        { exec "${CMD[@]}"; } >"$STATE/out" 2>&1 &
        pid=$!
        for _ in $(seq 1 400); do [[ -e "$STATE/$flag" ]] && break; sleep 0.05; done
        kill "-$sig" -- "-$pid"
        wait "$pid"
    ) 2>/dev/null
    RC=$?
    tripwire
}
# A stop is not a run's outcome: each signal keeps its own code (and the unit
# ends failed, which shows it), the run is recorded as failed with the
# signal named, and no report is sent — nothing anywhere says "exited 3".
# HUP (a terminal hanging up), PIPE (a stream that went away), USR1 and ALRM
# used to read as "a command that failed": exit 3 and an ABORTED mail, while
# bash then died by the signal anyway (round 3, M2).
for spec in TERM:143 INT:130 HUP:129 PIPE:141 USR1:138 ALRM:142; do
    sig="${spec%%:*}" code="${spec#*:}"
    fresh
    knob btrbk_blocks 1
    run_signalled "$sig"
    check "SIG$sig while btrbk runs: exit status" "$RC" "$code"
    show_tail "$code"
    check "SIG$sig: cleanup() ran its recovery body" \
        "$(grep -c 'Cleaning up after abnormal termination' "$STATE/out")" "1"
    check "SIG$sig: recorded as failed" "$(recorded_as)" "failure"
    check "SIG$sig: the stop in the errors" \
        "$(vector_value --errors | grep -cxF "stopped: by SIG$sig (exit $code)")" "1"
    check "SIG$sig: nothing left mounted" "$(left_mounted)" "nothing"
    check "SIG$sig: no mail" "$(mails)" "0"
    check "SIG$sig: nothing says exit 3" "$(grep -c 'exiting 3\|exited 3' "$STATE/out")" "0"
done

# The stream stdout writes to goes away mid-run (a terminal closed under
# `| tee`, say): the run's next line raises SIGPIPE, a stop. cleanup()'s own
# lines then fail as well; they must not end it before it records and
# unmounts (it ignores PIPE from its first line). The log file keeps every
# line either way.
fresh
knob btrbk_blocks 1
run_cmd
mkfifo "$WORK/stream"
cat "$WORK/stream" >"$STATE/out" &
reader=$!
(
    set -m
    { exec "${CMD[@]}"; } >"$WORK/stream" 2>&1 &
    pid=$!
    for _ in $(seq 1 400); do [[ -e "$STATE/btrbk_started" ]] && break; sleep 0.05; done
    kill "$reader"                                        # the stream goes away
    pkill -x -P "$(cat "$STATE/btrbk_stub.pid")" sleep    # btrbk finishes; the run writes again
    wait "$pid"
) 2>/dev/null
RC=$?
wait "$reader" 2>/dev/null
rm -f "$WORK/stream"
tripwire
check "stdout gone mid-run: exit status (SIGPIPE)" "$RC" "141"
check "stdout gone mid-run: recorded as failed" "$(recorded_as)" "failure"
check "stdout gone mid-run: the stop in the errors" \
    "$(vector_value --errors | grep -cxF "stopped: by SIGPIPE (exit 141)")" "1"
check "stdout gone mid-run: nothing left mounted" "$(left_mounted)" "nothing"
check "stdout gone mid-run: no mail" "$(mails)" "0"
check "stdout gone mid-run: the log file has cleanup()'s line" \
    "$(grep -c 'Cleaning up after abnormal termination' "$WORK/log/das-backup.log")" "1"

# A stop in a dry run keeps its code too, and a dry run records nothing.
fresh
knob btrbk_blocks 1
run_signalled TERM --dryrun
check "SIGTERM in a dry run: exit status" "$RC" "143"
show_tail 143
check "SIGTERM in a dry run: not recorded" "$(record_calls)" "0"
check "SIGTERM in a dry run: no mail" "$(mails)" "0"

# A stop while the run waits behind another holder of the maintenance lock:
# it had not begun (oeo), so its code is kept and nothing is recorded, sent
# or unmounted — the stopped unit shows it.
fresh
(
    exec 7<>"$RUN_DIR/das-maintenance.lock"
    flock 7
    printf 'btrdasd scrub run pid 77\n' >&7
    : >"$STATE/mholding"
    exec sleep 60 # the holder is this pid, so killing it releases the lock
) &
HOLDER_PID=$!
for _ in $(seq 1 200); do [[ -e "$STATE/mholding" ]] && break; sleep 0.05; done
run_cmd
(
    set -m
    { exec "${CMD[@]}"; } >"$STATE/out" 2>&1 &
    pid=$!
    # Announced after its first 5 s probe.
    for _ in $(seq 1 300); do grep -q 'waiting\.\.\.' "$STATE/out" 2>/dev/null && break; sleep 0.05; done
    kill -TERM -- "-$pid"
    wait "$pid"
) 2>/dev/null
RC=$?
tripwire
kill "$HOLDER_PID" 2>/dev/null
wait "$HOLDER_PID" 2>/dev/null
HOLDER_PID=""
check "SIGTERM while waiting for the maintenance lock: exit status" "$RC" "143"
show_tail 143
check "SIGTERM while waiting: it was waiting" "$(grep -c 'waiting\.\.\.' "$STATE/out")" "1"
check "SIGTERM while waiting: cleanup() skipped its recovery body" \
    "$(grep -c 'Cleaning up after abnormal termination' "$STATE/out")" "0"
check "SIGTERM while waiting: not recorded" "$(record_calls)" "0"
check "SIGTERM while waiting: no mail" "$(mails)" "0"
check "SIGTERM while waiting: nothing mounted" "$(called mount)" "no"

# ---------------------------------------------------------------------------
echo "== source volumes: a run unmounts only what it mounted (bd DAS-Backup-Manager-8cf)"
# ---------------------------------------------------------------------------
# A run never owns a mount it found in place. A source volume already mounted
# when the run looks — fstab mounts the operator's general-use /dasRaid0, and
# the /.btrfs-* top levels — is used as found and never unmounted: not at the
# end, not by cleanup() after an abort or a stop, not in a dry run. One the
# run mounted itself is unmounted exactly once, however many sources share
# it. The run used to unmount every source: each night it took down the
# /.btrfs-* fstab mounts and tried /dasRaid0. Verification is unchanged: a
# source found mounted must still be the expected filesystem at its top
# level, or the run aborts — leaving the mount it refused as it found it.
DAS_STORAGE_MNT="$WORK/mnt/dasRaid0"

# premount <path> <uuid> [fsroot]: mounted before the run starts, as fstab
# mounts it at boot.
premount() {
    mkdir -p "$1"
    printf '%s\t%s\t%s\n' "$1" "$2" "${3:-/}" >>"$STATE/mounted"
}
# calls_for <mount|umount> <path>: how many times the run called it for <path>.
calls_for() {
    if [[ -f "$STATE/calls/$1" ]]; then
        awk -v p="$2" '$NF == p { n++ } END { print n + 0 }' "$STATE/calls/$1"
    else
        echo 0
    fi
}
# second_source <label> <volume> <device>: a second source in the config.
second_source() {
    sed -i 's/^DAS_SOURCE_COUNT=1$/DAS_SOURCE_COUNT=2/' "$STATE/env"
    grep -qx 'DAS_SOURCE_COUNT=2' "$STATE/env" || harness_broken "could not add a second source to $STATE/env"
    cat >>"$STATE/env" <<EOF
DAS_SOURCE_1_LABEL='$1'
DAS_SOURCE_1_VOLUME='$2'
DAS_SOURCE_1_DEVICE='$3'
DAS_SOURCE_1_SUBVOLUMES='@data'
DAS_SOURCE_1_SNAPSHOT_DIR='.btrbk-snapshots'
DAS_SOURCE_1_TARGET_SUBDIRS='$1'
EOF
}
# The lines that say a source was used as found / unmounted by the run.
found_line() { grep -cF -- "$1: $2 was already mounted — used as found; this run will not unmount it" "$STATE/out"; }
unmounted_line() { grep -cF -- "Unmounted source volume $1 (this run mounted it)" "$STATE/out"; }

# --- found mounted: never the run's, on any way out ---------------------------
fresh
premount "$SOURCE_MNT" source-uuid
run_backup
check "found mounted, clean run: exit status" "$RC" "0"
show_tail 0
check "found mounted, clean run: report status" "$(report_status)" "ALL OPERATIONS SUCCESSFUL"
check "found mounted, clean run: says it is used as found" "$(found_line nvme "$SOURCE_MNT")" "1"
check "found mounted, clean run: not mounted again" "$(calls_for mount "$SOURCE_MNT")" "0"
check "found mounted, clean run: never unmounted" "$(calls_for umount "$SOURCE_MNT")" "0"
check "found mounted, clean run: left as found, the targets released" "$(left_mounted)" "$SOURCE_MNT "
check "found mounted, clean run: the DAS is safe to disconnect" \
    "$(grep -c 'DAS can be safely disconnected' "$STATE/out")" "1"

fresh
premount "$SOURCE_MNT" source-uuid
knob wrong_fs_at "$PRIMARY_MNT"
run_backup
check "found mounted, an abort: exit status" "$RC" "3"
show_tail 3
check "found mounted, an abort: cleanup() ran its recovery body" \
    "$(grep -c 'Cleaning up after abnormal termination' "$STATE/out")" "1"
check "found mounted, an abort: the ABORTED report" "$(mails) $(mail_status 1)" "1 ABORTED"
check "found mounted, an abort: never unmounted" "$(calls_for umount "$SOURCE_MNT")" "0"
check "found mounted, an abort: left as found, the targets released" "$(left_mounted)" "$SOURCE_MNT "

fresh
premount "$SOURCE_MNT" source-uuid
knob btrbk_blocks 1
run_signalled TERM
check "found mounted, SIGTERM while btrbk runs: exit status" "$RC" "143"
show_tail 143
check "found mounted, SIGTERM: cleanup() ran its recovery body" \
    "$(grep -c 'Cleaning up after abnormal termination' "$STATE/out")" "1"
check "found mounted, SIGTERM: never unmounted" "$(calls_for umount "$SOURCE_MNT")" "0"
check "found mounted, SIGTERM: left as found, the targets released" "$(left_mounted)" "$SOURCE_MNT "

fresh
premount "$SOURCE_MNT" source-uuid
run_backup --dryrun
check "found mounted, dry run: exit status" "$RC" "0"
show_tail 0
check "found mounted, dry run: btrbk dryrun ran" "$(ran_btrbk)" "yes"
check "found mounted, dry run: says it is used as found" "$(found_line nvme "$SOURCE_MNT")" "1"
check "found mounted, dry run: never unmounted" "$(calls_for umount "$SOURCE_MNT")" "0"
check "found mounted, dry run: left as found, the targets released" "$(left_mounted)" "$SOURCE_MNT "

fresh
premount "$SOURCE_MNT" source-uuid
knob wrong_fs_at "$PRIMARY_MNT"
run_backup --dryrun
check "found mounted, dry run aborted: exit status" "$RC" "3"
show_tail 3
check "found mounted, dry run aborted: never unmounted" "$(calls_for umount "$SOURCE_MNT")" "0"
check "found mounted, dry run aborted: left as found, the targets released" "$(left_mounted)" "$SOURCE_MNT "

# --- mounted by the run: unmounted exactly once, on every way out ---------------
fresh
run_backup
check "mounted by the run, clean run: exit status" "$RC" "0"
show_tail 0
check "mounted by the run, clean run: mounted once" "$(calls_for mount "$SOURCE_MNT")" "1"
check "mounted by the run, clean run: unmounted exactly once" "$(calls_for umount "$SOURCE_MNT")" "1"
check "mounted by the run, clean run: says so" "$(unmounted_line "$SOURCE_MNT")" "1"
check "mounted by the run, clean run: nothing left mounted" "$(left_mounted)" "nothing"

fresh
knob wrong_fs_at "$PRIMARY_MNT"
run_backup
check "mounted by the run, an abort: exit status" "$RC" "3"
show_tail 3
check "mounted by the run, an abort: unmounted exactly once" "$(calls_for umount "$SOURCE_MNT")" "1"
check "mounted by the run, an abort: nothing left mounted" "$(left_mounted)" "nothing"

# An abort after main()'s own unmount_all: cleanup() runs it a second time,
# which must not unmount the source again.
fresh
knob break_log_at_record "$WORK/log/das-backup.log"
run_backup
check "mounted by the run, an abort after main()'s unmount: exit status" "$RC" "3"
show_tail 3
check "mounted by the run, an abort after main()'s unmount: cleanup() ran its recovery body" \
    "$(grep -c 'Cleaning up after abnormal termination' "$STATE/out")" "1"
check "mounted by the run, an abort after main()'s unmount: still unmounted exactly once" \
    "$(calls_for umount "$SOURCE_MNT")" "1"

fresh
knob btrbk_blocks 1
run_signalled TERM
check "mounted by the run, SIGTERM while btrbk runs: exit status" "$RC" "143"
show_tail 143
check "mounted by the run, SIGTERM: unmounted exactly once" "$(calls_for umount "$SOURCE_MNT")" "1"
check "mounted by the run, SIGTERM: nothing left mounted" "$(left_mounted)" "nothing"

fresh
run_backup --dryrun
check "mounted by the run, dry run: exit status" "$RC" "0"
show_tail 0
check "mounted by the run, dry run: unmounted exactly once" "$(calls_for umount "$SOURCE_MNT")" "1"
check "mounted by the run, dry run: nothing left mounted" "$(left_mounted)" "nothing"

fresh
knob wrong_fs_at "$PRIMARY_MNT"
run_backup --dryrun
check "mounted by the run, dry run aborted: exit status" "$RC" "3"
show_tail 3
check "mounted by the run, dry run aborted: unmounted exactly once" "$(calls_for umount "$SOURCE_MNT")" "1"
check "mounted by the run, dry run aborted: nothing left mounted" "$(left_mounted)" "nothing"

# --- two sources sharing one mount point (nvme and nvme-vm: /.btrfs-nvme) -------
fresh
second_source nvme-vm "$SOURCE_MNT" UUID=source-uuid
run_backup
check "two sources, one mount point the run mounted: exit status" "$RC" "0"
show_tail 0
check "two sources, one mount point the run mounted: one mount, one umount" \
    "$(calls_for mount "$SOURCE_MNT") $(calls_for umount "$SOURCE_MNT")" "1 1"
check "two sources, one mount point the run mounted: nothing left mounted" "$(left_mounted)" "nothing"

fresh
second_source nvme-vm "$SOURCE_MNT" UUID=source-uuid
knob wrong_fs_at "$PRIMARY_MNT"
run_backup
check "two sources, one mount point the run mounted, an abort: exit status" "$RC" "3"
show_tail 3
check "two sources, one mount point the run mounted, an abort: one mount, one umount" \
    "$(calls_for mount "$SOURCE_MNT") $(calls_for umount "$SOURCE_MNT")" "1 1"

fresh
second_source nvme-vm "$SOURCE_MNT" UUID=source-uuid
premount "$SOURCE_MNT" source-uuid
run_backup
check "two sources, one mount point found mounted: exit status" "$RC" "0"
show_tail 0
check "two sources, one mount point found mounted: neither mounted nor unmounted" \
    "$(calls_for mount "$SOURCE_MNT") $(calls_for umount "$SOURCE_MNT")" "0 0"
check "two sources, one mount point found mounted: left as found" "$(left_mounted)" "$SOURCE_MNT "

# --- the live layout: /dasRaid0 is fstab's, a top level the run mounts ----------
fresh
second_source das-storage "$DAS_STORAGE_MNT" UUID=das-storage-uuid
premount "$DAS_STORAGE_MNT" das-storage-uuid
run_backup
check "live layout: exit status" "$RC" "0"
show_tail 0
check "live layout: /dasRaid0 neither mounted nor unmounted" \
    "$(calls_for mount "$DAS_STORAGE_MNT") $(calls_for umount "$DAS_STORAGE_MNT")" "0 0"
check "live layout: the helper mount made, and taken down once" \
    "$(calls_for mount "$SOURCE_MNT") $(calls_for umount "$SOURCE_MNT")" "1 1"
check "live layout: only /dasRaid0 left mounted" "$(left_mounted)" "$DAS_STORAGE_MNT "
check "live layout: no unmount warning" "$(grep -c 'Could not unmount' "$STATE/out")" "0"

# A source that will not mount stops the run; the one found mounted stays.
fresh
second_source das-storage "$DAS_STORAGE_MNT" UUID=das-storage-uuid
premount "$DAS_STORAGE_MNT" das-storage-uuid
knob mount_fails "$SOURCE_MNT"
run_backup
check "live layout, a source will not mount: exit status" "$RC" "3"
show_tail 3
check "live layout, a source will not mount: what aborted" "$(body_field 1 'What aborted')" "source mount"
check "live layout, a source will not mount: /dasRaid0 never unmounted" \
    "$(calls_for umount "$DAS_STORAGE_MNT")" "0"
check "live layout, a source will not mount: only /dasRaid0 left mounted" "$(left_mounted)" "$DAS_STORAGE_MNT "

# --- verification is unchanged: a wrong mount found in place is refused --------
fresh
premount "$SOURCE_MNT" a-different-filesystem
run_backup
check "found mounted, the wrong filesystem: exit status" "$RC" "3"
show_tail 3
check "found mounted, the wrong filesystem: refused" \
    "$(grep -c 'ABORTING — refusing to write to source volumes' "$STATE/out")" "1"
check "found mounted, the wrong filesystem: btrbk never ran" "$(ran_btrbk)" "no"
check "found mounted, the wrong filesystem: what aborted" "$(body_field 1 'What aborted')" "source verification"
check "found mounted, the wrong filesystem: why" \
    "$(grep -cF -- "nvme: $SOURCE_MNT has fs UUID 'a-different-filesystem', expected 'source-uuid'" "$STATE/mail.1.body")" "1"
check "found mounted, the wrong filesystem: not mounted over" "$(calls_for mount "$SOURCE_MNT")" "0"
check "found mounted, the wrong filesystem: left as found" \
    "$(calls_for umount "$SOURCE_MNT") $(left_mounted)" "0 $SOURCE_MNT "

fresh
premount "$SOURCE_MNT" source-uuid /@
run_backup
check "found mounted at a subvolume: exit status" "$RC" "3"
show_tail 3
check "found mounted at a subvolume: what aborted" "$(body_field 1 'What aborted')" "source verification"
check "found mounted at a subvolume: why" \
    "$(grep -cF -- "nvme: $SOURCE_MNT is mounted at subvolume '/@', expected the top-level volume '/'" "$STATE/mail.1.body")" "1"
check "found mounted at a subvolume: left as found" \
    "$(calls_for umount "$SOURCE_MNT") $(left_mounted)" "0 $SOURCE_MNT "

# --- a helper mount that will not unmount: a WARN with umount's own words -------
fresh
knob umount_fails_once "$SOURCE_MNT"
run_backup
check "helper will not unmount: exit status (a WARN, not a FAIL)" "$RC" "0"
show_tail 0
check "helper will not unmount: report status" "$(report_status)" "ALL OPERATIONS SUCCESSFUL"
check "helper will not unmount: recorded as a success" "$(recorded_as)" "success"
# The journal's copy has colour codes around its level; the log file's has not.
check "helper will not unmount: in the journal, with umount's own message" \
    "$(grep -cF -- "Could not unmount source volume $SOURCE_MNT, which this run mounted: umount: $SOURCE_MNT: target is busy (stub). — left mounted; best effort, not a DAS disconnect concern" "$STATE/out")" "1"
check "helper will not unmount: a WARN in the log file, with umount's own message" \
    "$(grep -cF -- "[WARN]   Could not unmount source volume $SOURCE_MNT, which this run mounted: umount: $SOURCE_MNT: target is busy (stub). — left mounted" "$WORK/log/das-backup.log")" "1"
check "helper will not unmount: tried once" "$(calls_for umount "$SOURCE_MNT")" "1"
check "helper will not unmount: left mounted, the targets released" "$(left_mounted)" "$SOURCE_MNT "
check "helper will not unmount: the DAS is still safe to disconnect" \
    "$(grep -c 'DAS can be safely disconnected' "$STATE/out")" "1"

# --- the probe cannot tell whether the helper is still mounted (review F1) -----
# A mountpoint error is not "not mounted": the helper used to be struck off
# its record and left mounted, with nothing said. Now the run says it could
# not tell, with the probe's message, and unmounts anyway.
fresh
knob probe_fails_after_btrbk "$SOURCE_MNT"
run_backup
check "probe cannot tell for the helper: exit status" "$RC" "0"
show_tail 0
check "probe cannot tell for the helper: unmounted anyway, once" "$(calls_for umount "$SOURCE_MNT")" "1"
check "probe cannot tell for the helper: nothing left mounted" "$(left_mounted)" "nothing"
check "probe cannot tell for the helper: says so, with the probe's own message" \
    "$(grep -cF -- "Could not tell whether source volume $SOURCE_MNT, which this run mounted, is still mounted — mountpoint: $SOURCE_MNT: Input/output error (stub) (exit 1); unmounting it anyway" "$WORK/log/das-backup.log")" "1"
check "probe cannot tell for the helper: a WARN" \
    "$(grep -c "\[WARN\]   Could not tell whether source volume" "$WORK/log/das-backup.log")" "1"

# --- a stop while a source's mount runs (review F2) -----------------------------
# The run records the mount point as its own BEFORE mount runs, so a stop that
# lands while mount runs still finds a mount mount made, and takes it down.
# Recorded after mount instead, the stop leaves it mounted.
fresh
knob mount_blocks_after "$SOURCE_MNT"
run_signalled_when mount_in TERM
check "SIGTERM while mount runs, the mount made: the stop landed there" \
    "$([[ -e "$STATE/mount_in" ]] && echo yes || echo no) $(ran_btrbk)" "yes no"
check "SIGTERM while mount runs, the mount made: exit status" "$RC" "143"
show_tail 143
check "SIGTERM while mount runs, the mount made: taken down once" "$(calls_for umount "$SOURCE_MNT")" "1"
check "SIGTERM while mount runs, the mount made: nothing left mounted" "$(left_mounted)" "nothing"

fresh
knob mount_blocks_before "$SOURCE_MNT"
run_signalled_when mount_in TERM
check "SIGTERM while mount runs, the mount not made: the stop landed there" \
    "$([[ -e "$STATE/mount_in" ]] && echo yes || echo no) $(ran_btrbk)" "yes no"
check "SIGTERM while mount runs, the mount not made: exit status" "$RC" "143"
show_tail 143
check "SIGTERM while mount runs, the mount not made: nothing to unmount" "$(calls_for umount "$SOURCE_MNT")" "0"
check "SIGTERM while mount runs, the mount not made: nothing left mounted" "$(left_mounted)" "nothing"

# --- fstab declares a source's mount point, and it is not mounted -------------
# fstab's own mount is missing: said once, as INFO — not a WARN, not a FAIL —
# and the run still takes its helper down. Nothing is said for one found
# mounted, nor for a path fstab does not declare.
fresh
second_source das-storage "$DAS_STORAGE_MNT" UUID=das-storage-uuid
premount "$DAS_STORAGE_MNT" das-storage-uuid
printf '%s\n' "$SOURCE_MNT" "$DAS_STORAGE_MNT" >"$STATE/knobs/fstab_declares"
run_backup
check "fstab declares it, not mounted: exit status" "$RC" "0"
show_tail 0
check "fstab declares it, not mounted: said, as INFO" \
    "$(grep -cF -- "[INFO]   nvme: fstab mounts $SOURCE_MNT at boot, but it was not mounted — this run mounts a helper there and takes it down at the end" "$WORK/log/das-backup.log")" "1"
check "fstab declares it, found mounted: nothing said" "$(grep -cF -- "fstab mounts $DAS_STORAGE_MNT" "$STATE/out")" "0"
check "fstab declares it, not mounted: the helper still taken down; fstab's other mount left" \
    "$(calls_for umount "$SOURCE_MNT") $(left_mounted)" "1 $DAS_STORAGE_MNT "
check "fstab declares it, not mounted: the report is unchanged" "$(report_status)" "ALL OPERATIONS SUCCESSFUL"

fresh
run_backup
check "fstab does not declare it: nothing said" "$(grep -c 'fstab mounts' "$STATE/out")" "0"

# ---------------------------------------------------------------------------
echo "== \"DAS can be safely disconnected\" only when every target is known released (bd DAS-Backup-Manager-jug6)"
# ---------------------------------------------------------------------------
# The disconnect claim stands on the target unmount gate. A mountpoint error
# on a target used to read as "not mounted": the target was skipped, the
# report said "Unmount targets OK" and the run said the DAS could be
# disconnected while a drive was mounted — the operator might pull it on that
# word. "Could not tell" now fails the gate: the run says it is NOT safe to
# disconnect and why, and tries the unmount anyway.
unmount_row() { sed -n '/^  Unmount targets /{p;q;}' "$WORK/lib/last-report.txt" 2>/dev/null; }

fresh
knob probe_fails_after_btrbk "$PRIMARY_MNT"
# The boot step does nothing unless [boot] is enabled (dtm): enable it here.
knob boot_enabled true
write_env
run_backup
check "probe cannot tell for a mounted target: exit status (a FAIL)" "$RC" "3"
show_tail 3
check "probe cannot tell for a mounted target: unmounted anyway" \
    "$(calls_for umount "$PRIMARY_MNT") $(left_mounted)" "1 nothing"
check "probe cannot tell for a mounted target: never says the DAS is safe to disconnect" \
    "$(grep -c 'DAS can be safely disconnected' "$STATE/out")" "0"
check "probe cannot tell for a mounted target: says it is NOT safe, and why" \
    "$(grep -cF -- "DAS is NOT safe to disconnect: could not tell whether $PRIMARY_MNT is mounted — mountpoint: $PRIMARY_MNT: Input/output error (stub) (exit 1); umount then succeeded" "$STATE/out")" "1"
check "probe cannot tell for a mounted target: the report's row" \
    "$(unmount_row)" \
    "  Unmount targets       FAIL  (could not tell whether $PRIMARY_MNT is mounted — mountpoint: $PRIMARY_MNT: Input/output error (stub) (exit 1); umount then succeeded)"
check "probe cannot tell for a mounted target: the report's status" "$(report_status)" "FAILURES DETECTED"
check "probe cannot tell for a mounted target: recorded as failed, with the reason" \
    "$(recorded_as) $(vector_value --errors | grep -c "^unmount: could not tell whether $PRIMARY_MNT is mounted")" "failure 1"
# The same probe error reaches the boot-subvolume step, which runs after btrbk
# and before the unmount (bd DAS-Backup-Manager-jlsz): it read the error as
# "not mounted", skipped the target uncounted and recorded OK, with nothing said.
# Now it fails the step, names the target and the probe's message, and touches
# nothing on it.
boot_row() { sed -n '/^  Boot subvolumes /{p;q;}' "$WORK/lib/last-report.txt" 2>/dev/null; }
check "probe cannot tell for a mounted target: the boot-subvolume step says so, naming the target" \
    "$(grep -cF -- "Could not tell whether $PRIMARY_MNT is mounted — mountpoint: $PRIMARY_MNT: Input/output error (stub) (exit 1); its boot subvolumes were NOT updated" "$STATE/out")" "1"
check "probe cannot tell for a mounted target: the report's boot-subvolume row is a FAIL" \
    "$(boot_row)" "  Boot subvolumes       FAIL  (0 updated, 1 failed)"
check "probe cannot tell for a mounted target: the boot-subvolume failure is in the history row" \
    "$(vector_value --errors | grep -c '^boot_subvols: 0 updated, 1 failed')" "1"

# A target that will not unmount: NOT safe, and the same line says which.
fresh
knob umount_fails_always "$PRIMARY_MNT"
run_backup
check "a target that will not unmount: exit status" "$RC" "3"
show_tail 3
check "a target that will not unmount: says it is NOT safe, and which is still mounted" \
    "$(grep -cF -- "DAS is NOT safe to disconnect: still mounted: $PRIMARY_MNT" "$STATE/out")" "1"
check "a target that will not unmount: the report's row" \
    "$(unmount_row)" "  Unmount targets       FAIL  (still mounted: $PRIMARY_MNT)"

# A clean probe: unchanged.
fresh
run_backup
check "a clean probe: the report's row" "$(unmount_row)" "  Unmount targets       OK  (all clean)"
check "a clean probe: safe to disconnect, and nothing says otherwise" \
    "$(grep -c 'DAS can be safely disconnected' "$STATE/out") $(grep -c 'NOT safe to disconnect\|Could not tell' "$STATE/out")" "1 0"

# An absent drive's mount point is removed on purpose: mountpoint answers it
# 1 ("No such file or directory"), as it answers an error, and it is not one.
fresh
printf '%s\n' primary-uuid >"$STATE/knobs/present_uuids"
run_backup
check "an absent target: its mount point is not there" "$([[ -e "$RECOVERY_MNT" ]] && echo yes || echo no)" "no"
check "an absent target: the unmount gate passes" "$(unmount_row)" "  Unmount targets       OK  (all clean)"
check "an absent target: no 'could not tell', and safe to disconnect" \
    "$(grep -c 'Could not tell' "$STATE/out") $(grep -c 'DAS can be safely disconnected' "$STATE/out")" "0 1"

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
        # No signal reached the run: a command's own status, whatever its
        # number — a pipeline's SIGPIPE (141) or a child killed by TERM (143)
        # is a command that failed, not a stop.
        for rc in 0 1 2 32 127 129 130 138 141 142 143; do
            want="$([[ "$armed" == true ]] && echo 3 || echo 1)"
            check "abort_exit_status $rc, lock held=$armed, no signal" \
                "$(abort_exit_status "$rc" "$armed" "")" "$want"
        done
        # A signal the run itself received keeps its code.
        for spec in HUP:129 INT:130 USR1:138 PIPE:141 ALRM:142 TERM:143; do
            check "abort_exit_status ${spec#*:}, lock held=$armed, SIG${spec%%:*}" \
                "$(abort_exit_status "${spec#*:}" "$armed" "${spec%%:*}")" "${spec#*:}"
        done
    done
fi

# The units that treat 3 as success, and only 3, are the ones `btrdasd setup`
# writes, pinned by the Rust tests in indexer/src/setup/templates.rs. This
# suite also compared the packaged systemd/das-backup*.service.in until
# bd DAS-Backup-Manager-7rf, which stopped CMake installing backup units; that
# none is installed is checked by tests/test_install_destdir.sh.

# ---------------------------------------------------------------------------
echo "== 5bwi/nqbb: a boot-step WARN is a completed run (end to end)"
# ---------------------------------------------------------------------------
# The stub btrfs lists nothing (the mirror target is skipped: 1 skipped), so with [boot] enabled neither subvolume has a
# snapshot to build from: the step is a WARN. A WARN is not a failure — the
# report says COMPLETED WITH WARNINGS and the run exits 0, the unit staying
# green. Mapped by the generic any_op_is path, shown here for this step.
fresh
knob boot_enabled true
write_env
run_backup
check "boot step WARN, whole run: exit status 0" "$RC" "0"
show_tail 0
check "boot step WARN, whole run: the report's boot row is a WARN" \
    "$(boot_row)" "  Boot subvolumes       WARN  (0 updated, 1 skipped, 2 warnings)"
check "boot step WARN, whole run: the report's status" "$(report_status)" "COMPLETED WITH WARNINGS"
check "boot step WARN, whole run: recorded as a success" "$(recorded_as)" "success"

echo
echo "passed=$pass failed=$fail"
if ((NOT_RUN > 0)); then
    # Visible, never a pass: ctest sets REAL_BTRDASD to the binary CMake built.
    echo "NOT RUN: $NOT_RUN replay(s) of an abort's record-run vector through the real btrdasd —" \
        "set REAL_BTRDASD to a built btrdasd to run them"
fi
[[ $fail -eq 0 ]] || exit 1
echo "BACKUP EXIT SEMANTICS SUITE GREEN"
