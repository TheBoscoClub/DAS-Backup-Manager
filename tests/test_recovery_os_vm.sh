#!/bin/bash
# tests/test_recovery_os_vm.sh -- scripts/recovery-os-vm.sh against stubs
# (bd DAS-Backup-Manager-7wb).
#
# The driver runs unmodified, as installed (a copy beside a copy of
# packaging/libvirt/recovery-os-updater.xml), with every host path under a
# mktemp root (DAS_RECOVERY_VM_TEST_ROOT) and these commands stubbed on PATH:
# virsh, lsblk, systemctl, btrfs, btrdasd, logger, stat, id, magick, and
# flock -- which only logs and then runs the real flock, so the maintenance
# lock is a real lock: "held" and "released" below are observed with
# flock -n, never assumed. setsid is the real one, and the stub holder is a
# real process, so "the claim outlives the driver" is observed too.
#
# Every refusal is tested in its failing direction, beside the positive
# controls (a dry run and a full session) that prove the same checks pass a
# good drive. No root, no devices, no libvirt.
set -euo pipefail

# The interrupt tests need SIGINT deliverable. A shell started with it ignored
# -- an asynchronous command of a shell without job control, say, as a
# harness started with `(cmd &)` is -- can neither trap nor reset it, and
# neither can the drivers it starts: every interrupt test would fail for a
# reason that is not the driver's. Start again with the default dispositions.
if (((0x$(awk '/^SigIgn:/ {print $2}' /proc/$$/status) & 0x2) != 0)); then
    if [[ -n "${RECOVERY_OS_VM_SIGINT_RESET:-}" ]]; then
        echo "SIGINT is still ignored after resetting it: the interrupt tests cannot run here" >&2
        exit 1
    fi
    RECOVERY_OS_VM_SIGINT_RESET=1 exec env --default-signal=INT bash "$0" "$@"
fi

# The driver reads the boot record with jq and refuses every session without
# it; the one test about that ("no jq") takes jq off the driver's PATH on
# purpose. Without jq here, every other test would pass, or fail, through that
# refusal instead of the check it is about: stop instead of pretending.
if ! command -v jq >/dev/null; then
    echo "jq is not installed: the driver refuses every session without it, so this suite cannot test anything else -- install jq" >&2
    exit 1
fi

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REAL_FLOCK="$(command -v flock)"
export REAL_FLOCK

ROOTS=()
cleanup() {
    local root pid
    for root in "${ROOTS[@]}"; do
        # Every stub holder records its pid: kill by provenance, not by name.
        if [[ -f "$root/stub/holder.pids" ]]; then
            while read -r pid; do
                if [[ "$(tr '\0' ' ' 2>/dev/null <"/proc/$pid/cmdline")" == *"hold-disk"* ]]; then
                    kill -KILL "$pid" 2>/dev/null || :
                fi
            done <"$root/stub/holder.pids"
        fi
        if [[ -f "$root/stub/orphan.pid" ]]; then
            pid=$(<"$root/stub/orphan.pid")
            if [[ "$(tr '\0' ' ' 2>/dev/null <"/proc/$pid/cmdline")" == *"orphan-runner"* ]]; then
                kill -KILL "$pid" 2>/dev/null || :
            fi
        fi
        rm -rf -- "$root"
    done
}
trap cleanup EXIT

fails=0
passes=0
pass() {
    printf 'ok   %s\n' "$1"
    passes=$((passes + 1))
}
fail() {
    printf 'FAIL %s\n' "$1"
    fails=$((fails + 1))
}
check() {
    if [[ "$2" == "$3" ]]; then pass "$1"; else fail "$1: got '$2', want '$3'"; fi
}
has() {
    if [[ "$2" == *"$3"* ]]; then pass "$1"; else fail "$1: '$3' not found in:"$'\n'"$2"; fi
}
lacks() {
    if [[ "$2" != *"$3"* ]]; then pass "$1"; else fail "$1: '$3' found in:"$'\n'"$2"; fi
}
matches() {
    if [[ "$2" =~ $3 ]]; then pass "$1"; else fail "$1: '$2' does not match /$3/"; fi
}
RECORD_A='^recovery-os VM session system-recovery-A-2tb pid [0-9]+$'

DISK_A_NAME="ata-ST2000DM008-2FR102_ZK208Q77"
DISK_B_NAME="ata-ST2000DM008-2FR102_ZFL41DNY"
# The filesystems on the two drives' partitions 2 -- their mount_uuid, as on
# this host.
UUID_A="60b05268-7f8f-47b5-a38a-752576a1172a"
UUID_B="7c7ae72d-09d6-4086-b249-1ac60f21b73b"

# A fresh root: the installed layout, two recovery drives and one leg of the
# primary pair, the domain defined and shut off, nothing running.
fixture() {
    T="$(mktemp -d)"
    ROOTS+=("$T")
    S="$T/stub"
    DRIVER="$T/usr/lib/das-backup/recovery-os-vm.sh"
    LOCK="$T/run/das-maintenance.lock"
    STATE="$T/run/das-recovery-os-vm"
    BYID="$T/dev/disk/by-id"
    DISK_A="$BYID/$DISK_A_NAME"
    DISK_B="$BYID/$DISK_B_NAME"
    LOOP=""
    OS_STATE=""
    STATE_FILE="$T/var/lib/das-backup/recovery-os.json"
    SESSIONS="$T/var/lib/das-backup/recovery-os-vm-sessions"
    mkdir -p "$S" "$T/bin" "$T/run" "$BYID" "$T/usr/lib/das-backup/libvirt" "$T/usr/share/edk2/x64"
    cp "$REPO/scripts/recovery-os-vm.sh" "$DRIVER"
    cp "$REPO/packaging/libvirt/recovery-os-updater.xml" "$T/usr/lib/das-backup/libvirt/"
    printf 'CODE' >"$T/usr/share/edk2/x64/OVMF_CODE.4m.fd"
    printf 'VARS-TEMPLATE' >"$T/usr/share/edk2/x64/OVMF_VARS.4m.fd"
    local d
    for d in sdj sdj1 sdj2 sdk sdk1 sdk2 sdm sdm1; do : >"$T/dev/$d"; done
    ln -s ../../sdj "$DISK_A"
    ln -s ../../sdj1 "$DISK_A-part1"
    ln -s ../../sdj2 "$DISK_A-part2"
    ln -s ../../sdk "$DISK_B"
    ln -s ../../sdk1 "$DISK_B-part1"
    ln -s ../../sdk2 "$DISK_B-part2"
    ln -s ../../sdm "$BYID/ata-ST22000NM000C-3WC103_ZXA1R71M"
    ln -s ../../sdm1 "$BYID/ata-ST22000NM000C-3WC103_ZXA1R71M-part1"
    printf 'disk ZK208Q77\n' >"$S/lsblk.typeserial.sdj"
    printf 'disk ZFL41DNY\n' >"$S/lsblk.typeserial.sdk"
    printf 'disk ZXA1R71M\n' >"$S/lsblk.typeserial.sdm"
    printf 'part \n' >"$S/lsblk.typeserial.sdj1"
    printf '%s\n' "$UUID_A" >"$S/lsblk.uuid.sdj2"
    printf '%s\n' "$UUID_B" >"$S/lsblk.uuid.sdk2"
    touch "$S/defined"
    printf 'shut off\n' >"$S/states"
    printf 'running\nrunning\nrunning\nshut off\n' >"$S/states.running"
    : >"$S/events"
    dump_env ZK208Q77 ZFL41DNY
    write_state 3 "$(record_json system-recovery-A-2tb no)" "$(record_json system-recovery-B-2tb no)"
    write_stubs
}

# One drive's entry in the boot record btrdasd keeps (`recovery-os status
# --state-file`, schema 3), in the shape bd DAS-Backup-Manager-1yg defines:
# $1 label, $2 verdict (no, will, may -- or anything, to test), $3 seconds
# since it was checked (an hour if not given).
record_json() {
    local reasons='[]'
    case "$2" in
        will) reasons='["btrbk.timer starts btrbk.service, which runs btrbk at its next scheduled time after boot, with /etc/btrbk/btrbk.conf present"]' ;;
        may) reasons='["not read"]' ;;
    esac
    printf '"%s":{"checked_epoch":%s,"os":{"btrbk_at_boot":{"verdict":"%s","reasons":%s,"runners":[]},"enabled_units":{"state":"listed","units":[{"name":"fstrim.timer","dirs":["etc/systemd/system/timers.target.wants"]},{"name":"sshd.service","dirs":["etc/systemd/system/multi-user.target.wants"]}]},"btrbk_config":{"state":"absent"}},"error":null}' \
        "$1" "$(($(date +%s) - ${3:-3600}))" "$2" "$reasons"
}

# The test hatch reads its own boot record (DAS_RECOVERY_OS_STATE) and keeps
# its session times beside it, never the usual ones: $1 the verdict for
# system-recovery-A-2tb, then more labels to give a "no" record.
hatch_state() {
    local entries label
    entries="$(record_json system-recovery-A-2tb "$1")"
    shift
    for label in "$@"; do entries+=",$(record_json "$label" no)"; done
    mkdir -p "$T/hatch"
    printf '{"schema_version":3,"drives":{%s}}\n' "$entries" >"$T/hatch/recovery-os.json"
    OS_STATE="$T/hatch/recovery-os.json"
}

# The boot record file: $1 its schema_version, then the drives' entries.
write_state() {
    local schema=$1 IFS=,
    shift
    mkdir -p "$(dirname "$STATE_FILE")"
    printf '{"schema_version":%s,"drives":{%s}}\n' "$schema" "$*" >"$STATE_FILE"
}

# What `btrdasd config dump-env` prints for this host's three targets, in the
# KEY='VALUE' form of indexer/src/setup/env_export.rs. $1 and $2 are the
# serials of the two mirror targets, $3 the second one's label if not B's, $4
# the first one's mount_uuid if not its real one ('' for none).
dump_env() {
    cat >"$S/dump-env" <<EOF
DAS_VERSION='0.7.22.3'
DAS_TARGET_COUNT=3
DAS_TARGET_0_LABEL='primary-22tb'
DAS_TARGET_0_SERIAL='ZXA1R71M'
DAS_TARGET_0_SERIALS='ZXA1R71M ZXA1NYGZ'
DAS_TARGET_0_MOUNT_UUID='b2dbe07d-40b9-422e-8ccf-ef4931c40457'
DAS_TARGET_0_MOUNT='/mnt/backup-22tb'
DAS_TARGET_0_ROLE='primary'
DAS_TARGET_1_LABEL='system-recovery-A-2tb'
DAS_TARGET_1_SERIAL='${1%% *}'
DAS_TARGET_1_SERIALS='$1'
DAS_TARGET_1_MOUNT_UUID='${4-$UUID_A}'
DAS_TARGET_1_MOUNT='/mnt/backup-system-recovery-A'
DAS_TARGET_1_ROLE='mirror'
DAS_TARGET_2_LABEL='${3:-system-recovery-B-2tb}'
DAS_TARGET_2_SERIAL='$2'
DAS_TARGET_2_SERIALS='$2'
DAS_TARGET_2_MOUNT_UUID='$UUID_B'
DAS_TARGET_2_MOUNT='/mnt/backup-system-recovery-B'
DAS_TARGET_2_ROLE='mirror'
EOF
}

write_stubs() {
    cat >"$T/bin/virsh" <<'STUB'
#!/bin/bash
S="$STUB"
if [[ "${1:-}" == --connect ]]; then shift 2; fi
printf '%s\n' "$*" >>"$S/virsh.calls"
cmd=${1:-}
shift || :
nodomain() { echo "error: failed to get domain 'recovery-os-updater'" >&2; exit 1; }
case "$cmd" in
    dominfo)
        [[ -f "$S/defined" ]] || nodomain
        echo "Name:           recovery-os-updater"
        ;;
    domstate)
        [[ -f "$S/defined" ]] || nodomain
        if [[ -f "$S/domstate_fail" ]]; then echo "error: failed to connect to the hypervisor" >&2; exit 1; fi
        if [[ -f "$S/started" ]]; then
            # What the lock's record says while the session runs.
            line=$(head -n 1 "$DAS_RECOVERY_VM_TEST_ROOT/run/das-maintenance.lock")
            printf '%s\n' "$line" >>"$S/lock.records"
            pid=${line##* pid }
            if [[ "$line" == *" pid "* && -d "/proc/$pid" ]]; then echo alive; else echo dead; fi >>"$S/lock.records.alive"
            polls=$(($(cat "$S/polls" 2>/dev/null || echo 0) + 1))
            echo "$polls" >"$S/polls"
            # The drive re-enumerates at the second poll: its by-id link moves.
            if [[ -f "$S/reenumerate" && "$polls" == 2 ]]; then
                read -r link target <"$S/reenumerate"
                ln -sfn "$target" "$link"
            fi
        fi
        state=$(head -n 1 "$S/states")
        if (($(wc -l <"$S/states") > 1)); then
            tail -n +2 "$S/states" >"$S/states.next" && mv "$S/states.next" "$S/states"
        fi
        printf '%s\n\n' "$state"
        ;;
    dumpxml)
        [[ -f "$S/defined" ]] || nodomain
        printf "<domain type='kvm' id='1'>\n  <name>recovery-os-updater</name>\n"
        if [[ -f "$S/stored-os.xml" ]]; then cat "$S/stored-os.xml"; fi
        printf "  <devices>\n"
        if [[ -f "$S/attached.xml" ]]; then sed 's/^/    /' "$S/attached.xml"; fi
        printf "  </devices>\n</domain>\n"
        ;;
    attach-device)
        [[ "${3:-}" == --config ]] || { echo "stub: attach-device without --config" >&2; exit 98; }
        if [[ -f "$S/attach_fail" ]]; then echo "error: Failed to attach device" >&2; exit 1; fi
        cp "$2" "$S/attached.xml"
        cat "$2" >>"$S/attach.log"
        echo "virsh attach" >>"$S/events"
        ;;
    detach-disk)
        [[ "${3:-}" == --config ]] || { echo "stub: detach-disk without --config" >&2; exit 98; }
        if [[ -f "$S/detach_fail" ]]; then echo "error: Failed to detach disk" >&2; exit 1; fi
        if ! grep -qF "<source dev='$2'/>" "$S/attached.xml" 2>/dev/null; then
            echo "error: No disk found whose source path or target is $2" >&2
            exit 1
        fi
        rm -f "$S/attached.xml"
        echo "virsh detach" >>"$S/events"
        echo "Disk detached successfully"
        ;;
    start)
        echo "virsh start" >>"$S/events"
        head -n 1 "$DAS_RECOVERY_VM_TEST_ROOT/run/das-maintenance.lock" >"$S/lock.record.at_start"
        cat "$DAS_RECOVERY_VM_TEST_ROOT/var/lib/das-backup/recovery-os-vm-sessions" >"$S/sessions.at_start" 2>/dev/null || :
        if [[ -f "$S/start_fail" ]]; then echo "error: Failed to start domain 'recovery-os-updater'" >&2; exit 1; fi
        cp "$S/states.running" "$S/states"
        touch "$S/started"
        # The holder dies (killed, out of memory) once the VM runs.
        if [[ -f "$S/kill_holder_at_start" ]]; then kill -KILL "$(head -n 1 "$S/holder.pids")"; fi
        # ...and from now on something keeps the drive busy.
        if [[ -f "$S/busy_after_start" ]]; then touch "$S/hold_busy"; fi
        # Something on the host mounts a partition while the session runs.
        if [[ -f "$S/lsblk.mounts.sdj.after_start" ]]; then cp "$S/lsblk.mounts.sdj.after_start" "$S/lsblk.mounts.sdj"; fi
        echo "Domain 'recovery-os-updater' started"
        ;;
    shutdown)
        echo "virsh shutdown" >>"$S/events"
        if [[ -f "$S/states.after_shutdown" ]]; then cp "$S/states.after_shutdown" "$S/states"; fi
        echo "Domain 'recovery-os-updater' is being shutdown"
        ;;
    define)
        touch "$S/defined"
        cp "${!#}" "$S/defined.xml"
        rm -f "$S/stored-os.xml"
        echo "Domain 'recovery-os-updater' defined from ${!#}"
        ;;
    screenshot)
        printf 'P6\n1 1\n255\n...' >"$2"
        echo "Screenshot saved to $2, with type of image/x-portable-pixmap"
        ;;
    *)
        echo "stub virsh: unexpected '$cmd $*'" >&2
        echo "UNEXPECTED $cmd $*" >>"$S/forbidden"
        exit 99
        ;;
esac
STUB

    cat >"$T/bin/lsblk" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/lsblk.calls"
dev=${!#}
name=$(basename "$dev")
case "$*" in
    *"-o TYPE,SERIAL"*)
        cat "$STUB/lsblk.typeserial.$name" 2>/dev/null || { echo "lsblk: $dev: not a block device" >&2; exit 32; }
        ;;
    *"-o TYPE "*)
        cat "$STUB/lsblk.type.$name" 2>/dev/null || { echo "lsblk: $dev: not a block device" >&2; exit 32; }
        ;;
    *"-o UUID "*)
        cat "$STUB/lsblk.uuid.$name" 2>/dev/null || { echo "lsblk: $dev: not a block device" >&2; exit 32; }
        ;;
    *"-o NAME,MOUNTPOINTS"*)
        if [[ -f "$STUB/lsblk_mounts_fail" ]]; then echo "lsblk: $dev: failed to read" >&2; exit 1; fi
        if [[ -f "$STUB/lsblk.mounts.$name" ]]; then
            cat "$STUB/lsblk.mounts.$name"
        else
            printf '%s \n%s1 \n%s2 \n' "$name" "$name" "$name"
        fi
        ;;
    *) echo "stub lsblk: unexpected $*" >&2; exit 99 ;;
esac
STUB

    cat >"$T/bin/systemctl" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/systemctl.calls"
if [[ "$1" != is-active ]]; then echo "stub systemctl: unexpected $*" >&2; exit 99; fi
if [[ -f "$STUB/systemctl_fail" ]]; then echo "Failed to connect to bus" >&2; exit 1; fi
shift
rc=0
for unit in "$@"; do
    state=$(cat "$STUB/unit.$unit" 2>/dev/null || echo inactive)
    echo "$state"
    [[ "$state" == active ]] || rc=3
done
exit "$rc"
STUB

    cat >"$T/bin/btrfs" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/btrfs.calls"
# Whether the driver still holds the maintenance lock while it rescans.
if "$REAL_FLOCK" -n "$DAS_RECOVERY_VM_TEST_ROOT/run/das-maintenance.lock" true; then
    echo "btrfs scan (lock free)" >>"$STUB/events"
else
    echo "btrfs scan (lock held)" >>"$STUB/events"
fi
if [[ -f "$STUB/scan_fail" ]]; then echo "ERROR: device scan failed" >&2; exit 1; fi
exit 0
STUB

    # The stub holder: what `btrdasd recovery-os hold-disk` does, minus the
    # disk. It waits on a FIFO of its own instead of a child `sleep`, so no
    # child of it can keep the inherited lock open after it exits.
    cat >"$T/bin/btrdasd" <<'STUB'
#!/bin/bash
case "${1:-} ${2:-}" in
    "config dump-env")
        if [[ -f "$STUB/dumpenv_fail" ]]; then echo "Error: cannot read config" >&2; exit 1; fi
        cat "$STUB/dump-env"
        ;;
    "recovery-os hold-disk")
        # The real holder blocks SIGINT, SIGTERM and SIGHUP and waits for
        # them, so a SIGINT reaches it whatever disposition it inherited --
        # and bash starts asynchronous commands with SIGINT ignored, which a
        # shell cannot trap. Start from default dispositions, like the real
        # one, or a Ctrl-C that would end the real hold would miss this one.
        if [[ -z "${STUB_DEFAULT_SIGNALS:-}" ]]; then
            STUB_DEFAULT_SIGNALS=1 exec env --default-signal "$0" "$@"
        fi
        dev=$4
        echo "$$" >>"$STUB/holder.pids"
        echo "hold-disk $dev" >>"$STUB/holder.calls"
        if [[ -f "$STUB/hold_busy" ]]; then
            # Busy for one claim only.
            if [[ -f "$STUB/busy_once" ]]; then rm -f "$STUB/hold_busy"; fi
            echo "Error: cannot hold $dev: $dev is in use — mounted or held by another program" >&2
            exit 2
        fi
        stat=$(</proc/$$/stat)
        read -r _ _ _ sid _ <<<"${stat##*) }"
        if [[ "$sid" == "$$" ]]; then echo "holder in its own session" >>"$STUB/events"; fi
        pwd >"$STUB/holder.cwd"
        # (no lock file at all when a test starts a holder by hand)
        head -n 1 "$DAS_RECOVERY_VM_TEST_ROOT/run/das-maintenance.lock" 2>/dev/null >"$STUB/lock.record.at_hold" || :
        if "$REAL_FLOCK" -n "$DAS_RECOVERY_VM_TEST_ROOT/run/das-maintenance.lock" true; then
            echo "holder started (lock free)" >>"$STUB/events"
        else
            echo "holder started (lock held)" >>"$STUB/events"
        fi
        fifo="$STUB/holder.$$.fifo"
        mkfifo "$fifo"
        exec 7<>"$fifo"
        rm -f "$fifo"
        if [[ -f "$STUB/holder_ignores_term" ]]; then
            trap '' TERM INT HUP
        else
            trap 'echo "holder released" >>"$STUB/events"; echo "released $dev on SIGTERM" >&2; exit 0' TERM INT HUP
        fi
        echo "held $dev pid $$"
        while :; do read -r -t 1 -u 7 _ || :; done
        ;;
    *) echo "stub btrdasd: unexpected $*" >&2; exit 99 ;;
esac
STUB

    cat >"$T/bin/logger" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/logger.calls"
STUB

    cat >"$T/bin/sync" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/sync.calls"
STUB

    cat >"$T/bin/stat" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/stat.calls"
cat "$STUB/stat.$(basename "${!#}")" 2>/dev/null || { echo "stat: cannot statx '${!#}': No such file or directory" >&2; exit 1; }
STUB

    cat >"$T/bin/id" <<'STUB'
#!/bin/bash
if [[ -f "$STUB/not_root" ]]; then echo 1000; else echo 0; fi
STUB

    cat >"$T/bin/magick" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/magick.calls"
out=${!#}
printf 'PNG' >"${out#png:}"
STUB

    cat >"$T/bin/flock" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/flock.calls"
exec "$REAL_FLOCK" "$@"
STUB

    # systemd-run --scope registers a transient scope holding its own pid and
    # then execs the command in place: the same pid, every open descriptor
    # kept (measured with a --user scope; see the 7wb report). The stub does
    # the second half. Variants: it fails, or it forks (so the pid it leaves
    # in $! is not the holder's).
    cat >"$T/bin/systemd-run" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/systemd-run.calls"
if [[ -f "$STUB/systemd_run_orphan" ]]; then
    (exec -a "orphan-runner $STUB" sleep 30) &
    echo $! >"$STUB/orphan.pid"
    echo "Failed to start transient scope unit: Connection timed out" >&2
    exit 1
fi
if [[ -f "$STUB/systemd_run_fail" ]]; then
    echo "Failed to start transient scope unit: Unit das-recovery-os-holder.scope already exists." >&2
    exit 1
fi
scope=false unit=""
while (($#)); do
    case "$1" in
        --scope) scope=true ;;
        --unit=*) unit=${1#--unit=} ;;
        --quiet | --expand-environment=no) ;;
        --) shift; break ;;
        *) echo "stub systemd-run: unexpected option $1" >&2; exit 98 ;;
    esac
    shift
done
if [[ "$scope" != true || -z "$unit" ]]; then echo "stub systemd-run: needs --scope and --unit" >&2; exit 98; fi
echo active >"$STUB/unit.$unit.scope"
if [[ -f "$STUB/systemd_run_forks" ]]; then
    "$@" &
    wait $!
    exit $?
fi
exec "$@"
STUB

    chmod +x "$T/bin/"*
}

driver_env() {
    env -u DAS_RECOVERY_OS_STATE ${OS_STATE:+DAS_RECOVERY_OS_STATE="$OS_STATE"} \
        PATH="${DRIVER_PATH:-$T/bin:$PATH}" STUB="$S" REAL_FLOCK="$REAL_FLOCK" \
        DAS_RECOVERY_VM_TEST_ROOT="$T" BTRDASD_BIN="$T/bin/btrdasd" \
        DAS_RECOVERY_VM_POLL_SECS="${POLL:-0.05}" DAS_RECOVERY_VM_MINUTE_SECS=1 DAS_RECOVERY_VM_GRACE_SECS=1 \
        ${LOOP:+DAS_RECOVERY_VM_TEST_LOOP="$LOOP"} "$@"
}

# Run the driver to completion: OUT is stdout and stderr together, RC its
# status. Bounded, so a driver that hangs fails its test instead of the suite
# never ending (124 or 137 then, never an expected status).
run_driver() {
    RC=0
    OUT="$(driver_env timeout -k 5 60 bash "$DRIVER" "$@" 2>&1)" || RC=$?
}

# A PATH with the stubs and only the real tools the driver needs up to its
# check for systemd-run -- none of which is systemd-run.
minimal_path() {
    local t
    mkdir -p "$T/sysbin"
    for t in bash env timeout readlink dirname basename cat head tr; do
        ln -sf "$(command -v "$t")" "$T/sysbin/$t"
    done
    printf '%s' "$T/bin:$T/sysbin"
}

# How frb's readers name the lock's holder -- maintenance_holder() in its
# backup-run.sh, holder_from_note() in its maintenance.rs: the record's first
# line, and whether the pid it ends with still runs.
frb_reader() {
    local line="" pid
    IFS= read -r line <"$LOCK" || :
    if [[ -z "$line" ]]; then
        echo "an unknown holder"
        return
    fi
    pid="${line##* pid }"
    if [[ "$line" != *" pid "* || ! "$pid" =~ ^[0-9]+$ ]]; then
        echo "an unknown holder (last recorded: $line)"
    elif [[ -d "/proc/$pid" ]]; then
        echo "$line"
    else
        echo "an unknown holder (the last recorded holder, $line, is no longer running)"
    fi
}

lock_state() {
    if "$REAL_FLOCK" -n "$LOCK" true; then echo free; else echo held; fi
}

file() { cat "$1" 2>/dev/null || :; }

# Another job holding the maintenance lock. The marker is written only once
# its flock has succeeded -- inside an `if`, so set -e cannot end the
# subshell before it sleeps -- and the tests wait for that marker, never for
# a lock state that something else could produce.
start_blocker() {
    (
        exec 9<>"$LOCK"
        if "$REAL_FLOCK" -n 9; then : >"$S/blocker.locked"; fi
        exec sleep 30
    ) &
    blocker=$!
    for ((i = 0; i < 200; i++)); do [[ -e "$S/blocker.locked" ]] && break; sleep 0.05; done
}
blocker_holds() {
    if [[ -e "$S/blocker.locked" ]]; then lock_state; else echo "the blocker never took the lock"; fi
}
stop_blocker() {
    kill "$blocker" 2>/dev/null || :
    wait "$blocker" 2>/dev/null || :
}
events() { tr '\n' '|' <"$S/events"; }
holder_pid() { head -n 1 "$STATE/system-recovery-$1-2tb.holder" 2>/dev/null || :; }
alive() {
    if [[ -n "$1" && "$(tr '\0' ' ' 2>/dev/null <"/proc/$1/cmdline")" == *hold-disk* ]]; then echo alive; else echo gone; fi
}

# The driver in the background, then Ctrl-C as a terminal sends it: SIGINT to
# its whole process group, once it is waiting for the guest. `set -m` gives
# the job a process group of its own (and keeps SIGINT deliverable to an
# asynchronous command at all). The holder, in its own session, is not in
# that group -- which is the point.
run_interrupted() {
    local i dpid
    RC=0
    set -m
    env -u DAS_RECOVERY_OS_STATE PATH="$T/bin:$PATH" STUB="$S" REAL_FLOCK="$REAL_FLOCK" \
        DAS_RECOVERY_VM_TEST_ROOT="$T" BTRDASD_BIN="$T/bin/btrdasd" \
        DAS_RECOVERY_VM_POLL_SECS="${POLL:-0.05}" DAS_RECOVERY_VM_MINUTE_SECS=1 DAS_RECOVERY_VM_GRACE_SECS=1 \
        bash "$DRIVER" "$@" >"$T/driver.out" 2>&1 &
    dpid=$!
    set +m
    for ((i = 0; i < 200; i++)); do
        if grep -q 'waiting for the recovery OS to power off' "$T/driver.out"; then break; fi
        sleep 0.05
    done
    sleep 0.2
    kill -INT -- "-$dpid" 2>/dev/null || echo "(the driver had exited before SIGINT)" >>"$T/driver.out"
    for ((i = 0; i < 600; i++)); do
        kill -0 "$dpid" 2>/dev/null || break
        sleep 0.05
    done
    if kill -0 "$dpid" 2>/dev/null; then
        kill -KILL -- "-$dpid" 2>/dev/null || :
        echo "(the driver did not exit within 30s of SIGINT)" >>"$T/driver.out"
    fi
    wait "$dpid" || RC=$?
    OUT="$(cat "$T/driver.out")"
}

# ============================================================================
echo "--- usage"
fixture
run_driver
check "no command: usage, exit 2" "$RC" "2"
run_driver frobnicate
check "unknown command: exit 2" "$RC" "2"
run_driver session
check "session without a target: exit 2" "$RC" "2"
run_driver session A --timeout 0
check "--timeout 0: exit 2" "$RC" "2"
run_driver session A --timeout soon
check "--timeout soon: exit 2" "$RC" "2"
run_driver session A --dry-run --timeout 5
check "--timeout with --dry-run: exit 2" "$RC" "2"
has "--timeout with --dry-run: said why" "$OUT" "no meaning with --dry-run"
run_driver session A B
check "two targets: exit 2" "$RC" "2"
run_driver session-end ''
check "session-end with an empty label: exit 2" "$RC" "2"
run_driver screenshot ''
check "screenshot with an empty path: exit 2" "$RC" "2"
POLL=0 run_driver session A --dry-run
check "a poll interval of 0: exit 2" "$RC" "2"
POLL=0.0 run_driver session A --dry-run
check "a poll interval of 0.0: exit 2" "$RC" "2"
run_driver --help
check "--help: exit 0" "$RC" "0"
has "--help: the commands" "$OUT" "session-end <A|B|label>"
check "usage errors touched nothing" "$(file "$S/virsh.calls")$(file "$S/holder.calls")" ""

echo "--- the definition and the script agree"
xml="$REPO/packaging/libvirt/recovery-os-updater.xml"
check "domain name" "$(sed -n 's|.*<name>\(.*\)</name>.*|\1|p' "$xml")" \
    "$(sed -n 's/^readonly DOMAIN="\(.*\)"$/\1/p' "$REPO/scripts/recovery-os-vm.sh")"
has "host CPU passed through" "$(cat "$xml")" "<cpu mode='host-passthrough'>"
has "no Secure Boot" "$(cat "$xml")" "secure='no'"
has "VNC only on a libvirt-private socket" "$(cat "$xml")" "<listen type='socket'/>"
lacks "no VNC on a network address" "$(cat "$xml")" "<listen type='address'"
has "the NVRAM comes from libvirt's template" "$(cat "$xml")" "<nvram template='"
has "guest agent channel" "$(cat "$xml")" "name='org.qemu.guest_agent.0'"
lacks "no disk of its own" "$(cat "$xml")" "<disk"
# libvirt gives a domain defined with no boot element at all an os-level
# <boot dev='hd'/>, which then refuses the disk's per-device <boot order='1'/>
# when a session attaches it ("per-device boot elements cannot be used
# together with os/boot elements") -- found on the real host.
lacks "no boot device under <os>" "$(sed -n '/<os>/,/<\/os>/p' "$xml")" "<boot dev="
has "the NIC carries a per-device boot order, after the disk" \
    "$(sed -n '/<interface /,/<\/interface>/p' "$xml")" "<boot order='2'/>"
lacks "boot order 1 is left for the disk" "$(cat "$xml")" "<boot order='1'/>"
if command -v virt-xml-validate >/dev/null; then
    if virt-xml-validate "$xml" domain >/dev/null 2>&1; then pass "virt-xml-validate"; else fail "virt-xml-validate"; fi
fi

echo "--- not root"
fixture
touch "$S/not_root"
run_driver session A --dry-run
check "not root: refused" "$RC" "1"
has "not root: says so" "$OUT" "must run as root"
check "not root: nothing held" "$(file "$S/holder.calls")" ""

echo "--- which drive"
fixture
run_driver session nope --dry-run
check "unknown label: refused" "$RC" "1"
has "unknown label: names the mirrors" "$OUT" "no target labelled 'nope'"
has "unknown label: lists the mirror targets" "$OUT" "system-recovery-A-2tb, system-recovery-B-2tb"

fixture
# A record that would let it through: the role alone must refuse it.
write_state 3 "$(record_json system-recovery-A-2tb no)" "$(record_json primary-22tb no)"
run_driver session primary-22tb --dry-run
check "primary label: refused" "$RC" "1"
has "primary label: says why" "$OUT" "'primary-22tb' is a role = \"primary\" target"
check "primary label: no holder, no lock" "$(file "$S/holder.calls")$(file "$S/flock.calls")" ""

fixture
run_driver session C --dry-run
check "shorthand naming no mirror: refused" "$RC" "1"
has "shorthand naming no mirror: says so" "$OUT" "'C' matches 0"

# A capital A inside a word is not the shorthand A: only a dash-separated one.
fixture
dump_env ZK208Q77 ZFL41DNY Archive-B-2tb
run_driver session A --dry-run
check "shorthand: a dash-separated word, not any capital" "$RC" "0"
has "shorthand: A is still drive A" "$OUT" "<source dev='$DISK_A'/>"

fixture
dump_env ZXA1R71M ZFL41DNY # a mirror entry carrying a serial of the primary pair
run_driver session A --dry-run
check "primary serial on a mirror entry: refused" "$RC" "1"
has "primary serial: names its real owner" "$OUT" "serial ZXA1R71M belongs to 'primary-22tb'"
check "primary serial: never held" "$(file "$S/holder.calls")" ""

fixture
dump_env "ZK208Q77 ZZZ00000" ZFL41DNY
run_driver session A --dry-run
check "two serials on a mirror: refused" "$RC" "1"
has "two serials: says so" "$OUT" "lists 2 drive serials"

fixture
dump_env 'ZK2*' ZFL41DNY
run_driver session A --dry-run
check "a serial with a glob character: refused" "$RC" "1"
has "glob serial: says so" "$OUT" "has characters a drive serial does not"

fixture
ln -s ../../sdj "$BYID/ata-OTHER-MODEL_ZK208Q77"
run_driver session A --dry-run
check "two by-id matches: refused" "$RC" "1"
has "two by-id matches: says so" "$OUT" "2 disks match"

fixture
rm "$DISK_A"
run_driver session A --dry-run
check "drive not attached: refused" "$RC" "1"
has "drive not attached: says so" "$OUT" "matches nothing"

fixture
ln -sfn ../../sdj1 "$DISK_A"
run_driver session A --dry-run
check "by-id leading to a partition: refused" "$RC" "1"
has "partition: says so" "$OUT" "a 'part', not a whole disk"

fixture
printf 'disk ZFL41DNY\n' >"$S/lsblk.typeserial.sdj"
run_driver session A --dry-run
check "disk reporting another serial: refused" "$RC" "1"
has "serial mismatch: says so" "$OUT" "reports serial 'ZFL41DNY', not ZK208Q77"

# The drive's filesystem must be the one config mounts that target by.
fixture
printf '%s\n' "ffffffff-0000-4000-8000-000000000000" >"$S/lsblk.uuid.sdj2"
run_driver session A --dry-run
check "partition 2 carrying another filesystem: refused" "$RC" "1"
has "another filesystem: says which" "$OUT" "carries filesystem ffffffff-0000-4000-8000-000000000000, not $UUID_A"
check "another filesystem: never held" "$(file "$S/holder.calls")" ""

fixture
dump_env ZK208Q77 ZFL41DNY system-recovery-B-2tb ""
run_driver session A --dry-run
check "a mirror without mount_uuid: refused" "$RC" "1"
has "no mount_uuid: says how to get one" "$OUT" "has no mount_uuid"
check "no mount_uuid: never held" "$(file "$S/holder.calls")" ""

fixture
touch "$S/dumpenv_fail"
run_driver session A --dry-run
check "config unreadable: refused" "$RC" "1"

echo "--- preflight"
for unit in das-backup.service das-backup-full.service das-scrub.service das-backup-doctor.service; do
    fixture
    echo activating >"$S/unit.$unit"
    run_driver session A --dry-run
    check "$unit activating: refused" "$RC" "1"
    has "$unit activating: named" "$OUT" "$unit is activating"
    check "$unit activating: no lock taken" "$(file "$S/flock.calls")" ""
done
fixture
echo active >"$S/unit.das-scrub.service"
echo failed >"$S/unit.das-backup.service"
run_driver session A --dry-run
check "active scrub: refused" "$RC" "1"
lacks "a failed unit is not running: not listed" "$OUT" "das-backup.service is failed"

fixture
touch "$S/systemctl_fail"
run_driver session A --dry-run
check "unit states unreadable: refused" "$RC" "1"
has "unit states unreadable: says so" "$OUT" "cannot tell whether"

fixture
printf 'sdj \nsdj1 \nsdj2 /mnt/backup-system-recovery-A\n' >"$S/lsblk.mounts.sdj"
run_driver session A --dry-run
check "a partition mounted: refused" "$RC" "1"
has "a partition mounted: names it" "$OUT" "sdj2 on /mnt/backup-system-recovery-A"
check "a partition mounted: no lock taken" "$(file "$S/flock.calls")" ""

fixture
touch "$S/lsblk_mounts_fail"
run_driver session A --dry-run
check "mounts unreadable: refused (not read as unmounted)" "$RC" "1"

fixture
rm "$S/defined"
run_driver session A --dry-run
check "domain not defined: refused" "$RC" "1"
has "domain not defined: points at define" "$OUT" "is it defined?"

fixture
printf 'running\n' >"$S/states"
run_driver session A --dry-run
check "domain running: refused" "$RC" "1"
has "domain running: says so" "$OUT" "recovery-os-updater is running -- it must be shut off"

fixture
printf "<disk type='block' device='disk'>\n  <source dev='/dev/sdx'/>\n</disk>\n" >"$S/attached.xml"
run_driver session A --dry-run
check "a disk already attached: refused" "$RC" "1"
has "a disk already attached: names it" "$OUT" "already has a disk attached (/dev/sdx)"

echo "--- a previous session's holder"
fixture
mkdir -p "$STATE"
setsid env STUB="$S" REAL_FLOCK="$REAL_FLOCK" DAS_RECOVERY_VM_TEST_ROOT="$T" \
    "$T/bin/btrdasd" recovery-os hold-disk --device "$DISK_B" </dev/null >"$S/old.out" 2>&1 &
old=$!
for ((i = 0; i < 200; i++)); do grep -q '^held ' "$S/old.out" 2>/dev/null && break; sleep 0.05; done
check "live holder: precondition, the old holder announced its hold" "$(grep -c '^held ' "$S/old.out" || :)" "1"
printf '%s\n%s\n' "$old" "$DISK_B" >"$STATE/system-recovery-B-2tb.holder"
run_driver session A --dry-run
check "live holder: refused" "$RC" "1"
has "live holder: says how to finish" "$OUT" "session-end system-recovery-B-2tb"
kill -TERM "$old" 2>/dev/null || :
wait "$old" 2>/dev/null || :
run_driver session A --dry-run
check "stale record (holder gone): removed, session goes on" "$RC" "0"
has "stale record: said so" "$OUT" "removing the stale record"

echo "--- the maintenance lock is held"
fixture
printf 'backup-run.sh pid 4242\n' >"$LOCK"
start_blocker
check "lock held: precondition, another process holds the lock" "$(blocker_holds)" "held"
run_driver session A --dry-run
check "lock held: refused" "$RC" "1"
has "lock held: prints the holder line" "$OUT" "held by: backup-run.sh pid 4242"
has "lock held: tried without waiting" "$(file "$S/flock.calls")" "-n"
check "lock held: no holder started" "$(file "$S/holder.calls")" ""
check "lock held: nothing attached" "$(file "$S/attach.log")" ""
check "lock held: the holder's record left alone" "$(head -n 1 "$LOCK")" "backup-run.sh pid 4242"
stop_blocker

echo "--- the holder cannot claim the disk"
fixture
touch "$S/hold_busy"
run_driver session A
check "EBUSY: refused" "$RC" "1"
has "EBUSY: says why" "$OUT" "is in use"
has "EBUSY: the holder was asked" "$(file "$S/holder.calls")" "hold-disk $DISK_A"
check "EBUSY: nothing attached" "$(file "$S/attach.log")" ""
check "EBUSY: never started" "$(grep -c '^start' "$S/virsh.calls" || :)" "0"
check "EBUSY: lock released" "$(lock_state)" "free"
check "EBUSY: no record left" "$(ls -A "$STATE")" ""

echo "--- dry run"
fixture
run_driver session A --dry-run
check "dry run: exit 0" "$RC" "0"
has "dry run: prints the disk" "$OUT" "<source dev='$DISK_A'/>"
has "dry run: SATA" "$OUT" "<target dev='sda' bus='sata'/>"
has "dry run: lock taken while holding" "$(events)" "holder started (lock held)"
has "dry run: holder released" "$(events)" "holder released"
check "dry run: nothing attached" "$(file "$S/attach.log")" ""
check "dry run: never started" "$(grep -c '^start' "$S/virsh.calls" || :)" "0"
check "dry run: lock released" "$(lock_state)" "free"
check "dry run: no record left" "$(ls -A "$STATE")" ""
check "dry run: no rescan (nothing booted)" "$(file "$S/btrfs.calls")" ""

echo "--- a full session"
fixture
run_driver session A
check "session: exit 0" "$RC" "0"
attached="$(file "$S/attach.log")"
has "session: whole disk by id" "$attached" "<source dev='$DISK_A'/>"
has "session: block device, raw, no host cache, native AIO" "$attached" "<driver name='qemu' type='raw' cache='none' io='native'/>"
has "session: SATA" "$attached" "<target dev='sda' bus='sata'/>"
has "session: first in the boot order" "$attached" "<boot order='1'/>"
has "session: attached to the persistent definition" "$(file "$S/virsh.calls")" "attach-device recovery-os-updater"
check "session: events in order" "$(events)" \
    "holder in its own session|holder started (lock held)|virsh attach|virsh start|virsh detach|holder released|btrfs scan (lock held)|"
matches "session: the lock's record names the session while it holds" "$(file "$S/lock.record.at_hold")" "$RECORD_A"
check "session: the record emptied before the lock was let go" "$(file "$LOCK")" ""
check "session: the holder runs in /" "$(file "$S/holder.cwd")" "/"
check "session: the holder runs in a scope of its own" "$(head -n 1 "$S/systemd-run.calls")" \
    "--scope --unit=das-recovery-os-holder-system-recovery-A-2tb --quiet --expand-environment=no -- $T/bin/btrdasd recovery-os hold-disk --device $DISK_A"
check "session: once it holds, the lock's record names the holder" "$(file "$S/lock.record.at_start")" \
    "recovery-os VM session system-recovery-A-2tb pid $(head -n 1 "$S/holder.pids")"
has "session: the claim held throughout" "$OUT" "Claim         held throughout"
# One preflight read, then running x3 and shut off: polling stops at shut off.
check "session: domstate polled until shut off" "$(grep -c '^domstate' "$S/virsh.calls")" "5"
has "session: state change logged" "$OUT" "domain state: running -> shut off"
check "session: partition 2 rescanned" "$(file "$S/btrfs.calls")" "device scan $DISK_A-part2"
check "session: lock released" "$(lock_state)" "free"
check "session: no record left" "$(ls -A "$STATE")" ""
has "session: console shown, through libvirt" "$OUT" "virt-viewer --connect qemu:///system --attach recovery-os-updater"
has "session: summary" "$OUT" "Session done -- system-recovery-A-2tb"
has "session: summary says no partition mounted" "$OUT" "no partition mounted"
has "session: summary says the holder was stopped" "$OUT" "detached; holder stopped;"
has "session: logged to the journal" "$(file "$S/logger.calls")" "-t das-recovery-os-vm -- attached $DISK_A"
check "session: never destroyed" "$(file "$S/forbidden")" ""

echo "--- shorthand B"
fixture
run_driver session b --dry-run
check "b: exit 0" "$RC" "0"
has "b: drive B" "$OUT" "<source dev='$DISK_B'/>"

echo "--- start fails"
fixture
touch "$S/start_fail"
run_driver session A
check "start failure: exit 1" "$RC" "1"
has "start failure: says why" "$OUT" "virsh start failed"
has "start failure: detached and released" "$(events)" "virsh start|virsh detach|holder released|"
check "start failure: lock released" "$(lock_state)" "free"
check "start failure: not in the definition" "$(file "$S/attached.xml")" ""

echo "--- interrupted while the recovery OS runs"
fixture
printf 'running\n' >"$S/states.running"
run_interrupted session A
check "SIGINT: exit 3" "$RC" "3"
lacks "SIGINT: nothing detached" "$(file "$S/virsh.calls")" "detach"
check "SIGINT: never destroyed" "$(file "$S/forbidden")" ""
pid="$(holder_pid A)"
check "SIGINT: the holder outlives the driver" "$(alive "$pid")" "alive"
# ...and it is the holder the session started: Ctrl-C did not reach it, so
# nothing had to be claimed again (it would, without a session of its own).
check "SIGINT: the original holder survived (no claim taken again)" "$(grep -c '^hold-disk' "$S/holder.calls")" "1"
check "SIGINT: the lock outlives the driver (inherited)" "$(lock_state)" "held"
has "SIGINT: the disk is still the VM's" "$(file "$S/attached.xml")" "$DISK_A"
has "SIGINT: says how to finish" "$OUT" "session-end system-recovery-A-2tb"
matches "SIGINT: the record stays while the session holds the lock" "$(head -n 1 "$LOCK")" "$RECORD_A"
has "SIGINT: never stop the holder's scope" "$OUT" "Never 'systemctl stop' das-recovery-os-holder-system-recovery-A-2tb.scope"
check "SIGINT: the record names the holder, which outlives the driver" "$(head -n 1 "$LOCK")" \
    "recovery-os VM session system-recovery-A-2tb pid $pid"
check "SIGINT: a reader sees the holder alive, not a finished session" "$(frb_reader)" \
    "recovery-os VM session system-recovery-A-2tb pid $pid"

run_driver status
check "status: exit 0" "$RC" "0"
has "status: domain state" "$OUT" "recovery-os-updater: running"
has "status: attached disk" "$OUT" "Attached disk     $DISK_A"
has "status: holder alive" "$OUT" "system-recovery-A-2tb: pid $pid, alive"
has "status: lock holder" "$OUT" "held by: recovery-os VM session system-recovery-A-2tb"
has "status: the holder's scope" "$OUT" "scope das-recovery-os-holder-system-recovery-A-2tb.scope: active"

run_driver session-end A
check "session-end while running: refused" "$RC" "1"
has "session-end while running: says why" "$OUT" "never stops a running recovery OS"
check "session-end while running: holder kept" "$(alive "$pid")" "alive"
check "session-end while running: lock kept" "$(lock_state)" "held"

printf 'shut off\n' >"$S/states"
run_driver session-end system-recovery-A-2tb
check "session-end once shut off: exit 0" "$RC" "0"
has "session-end: detached, holder stopped, rescanned" "$(events)" "virsh detach|holder released|btrfs scan"
check "session-end: holder gone" "$(alive "$pid")" "gone"
check "session-end: lock released with the holder" "$(lock_state)" "free"
check "session-end: no record left" "$(ls -A "$STATE")" ""
check "session-end: the lock's record emptied while the holder still held it" "$(file "$LOCK")" ""
has "session-end: reports the lock" "$OUT" "DAS maintenance lock: free"

run_driver session-end A
check "session-end with nothing to end: exit 0" "$RC" "0"
has "session-end with nothing to end: says so" "$OUT" "nothing to end"

run_driver status
has "status when idle: no holder" "$OUT" "Holder            none"
has "status when idle: lock free" "$OUT" "Maintenance lock  free"

echo "--- --timeout, and a guest that will not shut down"
fixture
printf 'running\n' >"$S/states.running"
run_driver session A --timeout 1
check "timeout: exit 3" "$RC" "3"
check "timeout: asked once" "$(grep -c '^shutdown' "$S/virsh.calls")" "1"
check "timeout: never destroyed" "$(file "$S/forbidden")" ""
lacks "timeout: nothing detached" "$(file "$S/virsh.calls")" "detach"
pid="$(holder_pid A)"
check "timeout: claim kept" "$(alive "$pid")" "alive"
check "timeout: lock kept" "$(lock_state)" "held"
has "timeout: says why" "$OUT" "did not power off within"
printf 'shut off\n' >"$S/states"
run_driver session-end A
check "timeout: session-end finishes it" "$RC" "0"
check "timeout: then the lock is free" "$(lock_state)" "free"

fixture
printf 'running\n' >"$S/states.running"
printf 'shut off\n' >"$S/states.after_shutdown"
run_driver session A --timeout 1
check "timeout, guest shuts down: exit 0" "$RC" "0"
has "timeout, guest shuts down: given back" "$(events)" "virsh shutdown|virsh detach|holder released|btrfs scan (lock held)|"
check "timeout, guest shuts down: lock free" "$(lock_state)" "free"

echo "--- the disk cannot be given back"
fixture
touch "$S/detach_fail"
run_driver session A
check "detach fails: exit 4" "$RC" "4"
pid="$(holder_pid A)"
check "detach fails: claim kept" "$(alive "$pid")" "alive"
check "detach fails: lock kept" "$(lock_state)" "held"
has "detach fails: says so" "$OUT" "still in recovery-os-updater's definition"
rm "$S/detach_fail"
rm -f "$SESSIONS"
run_driver session-end A
check "detach fails: session-end finishes it" "$RC" "0"
matches "detach fails: session-end records the session's end" "$(file "$SESSIONS")" "^system-recovery-A-2tb [0-9]+$"
check "detach fails: lock free after" "$(lock_state)" "free"

fixture
touch "$S/holder_ignores_term"
run_driver session A
check "holder will not stop: exit 4" "$RC" "4"
pid="$(holder_pid A)"
check "holder will not stop: lock kept" "$(lock_state)" "held"
has "holder will not stop: says so" "$OUT" "did not exit within"
check "holder will not stop: the record names it" "$(head -n 1 "$LOCK")" \
    "recovery-os VM session system-recovery-A-2tb pid $pid"
# session-end empties the record before it stops the holder (once the holder
# is gone the lock may be someone else's); a holder that stays keeps the lock,
# so the record must name it again.
run_driver session-end A
check "session-end, holder will not stop: exit 4" "$RC" "4"
check "session-end, holder will not stop: lock kept" "$(lock_state)" "held"
check "session-end, holder will not stop: the record names it again" "$(head -n 1 "$LOCK")" \
    "recovery-os VM session system-recovery-A-2tb pid $pid"
kill -KILL "$pid" 2>/dev/null || :
for ((i = 0; i < 100; i++)); do [[ "$(lock_state)" == free ]] && break; sleep 0.05; done
check "holder killed: the kernel drops the lock with it" "$(lock_state)" "free"

echo "--- the holder dies while the recovery OS runs"
fixture
touch "$S/kill_holder_at_start"
{
    for ((i = 0; i < 60; i++)); do echo running; done
    echo "shut off"
} >"$S/states.running"
run_driver session A
check "holder lost: the session ends, flagged" "$RC" "5"
has "holder lost: said so" "$OUT" "is gone while the recovery OS may use"
check "holder lost: claimed again" "$(grep -c '^hold-disk' "$S/holder.calls")" "2"
has "holder lost: the new holder holds the lock too" "$(events)" \
    "virsh start|holder in its own session|holder started (lock held)|"
has "holder lost: then given back as usual" "$(events)" "virsh detach|holder released|btrfs scan (lock held)|"
has "holder lost: the summary says so" "$OUT" "LOST 1 time(s) and claimed again"
check "holder lost: the record names the new holder" "$(tail -n 1 "$S/lock.records")" \
    "recovery-os VM session system-recovery-A-2tb pid $(sed -n 2p "$S/holder.pids")"
has "holder lost: the new holder has a scope of its own" "$(sed -n 2p "$S/systemd-run.calls")" \
    "--unit=das-recovery-os-holder-system-recovery-A-2tb-2 "
check "holder lost: lock free at the end" "$(lock_state)" "free"

# The holder dies, then the driver is interrupted before its next check: the
# claim and the lock it leaves behind must be a live holder's, not nothing.
fixture
touch "$S/kill_holder_at_start"
printf 'running\n' >"$S/states.running"
POLL=2 run_interrupted session A
check "holder lost, then SIGINT: exit 3" "$RC" "3"
check "holder lost, then SIGINT: claimed again before leaving" "$(grep -c '^hold-disk' "$S/holder.calls")" "2"
pid="$(holder_pid A)"
check "holder lost, then SIGINT: the new holder outlives the driver" "$(alive "$pid")" "alive"
check "holder lost, then SIGINT: and keeps the lock" "$(lock_state)" "held"
printf 'shut off\n' >"$S/states"
run_driver session-end A
check "holder lost, then SIGINT: session-end finishes it" "$RC" "0"
check "holder lost, then SIGINT: lock free after" "$(lock_state)" "free"

# session-end for a session whose holder is long gone: the lock may belong to
# another job by now, so its record is not this session's to empty.
fixture
mkdir -p "$STATE"
printf '%s\n%s\n' 999999999 "$DISK_A" >"$STATE/system-recovery-A-2tb.holder"
printf 'btrdasd walk pid 777\n' >"$LOCK"
start_blocker
check "session-end, holder gone: precondition, another process holds the lock" "$(blocker_holds)" "held"
run_driver session-end A
check "session-end, holder gone: exit 0" "$RC" "0"
has "session-end, holder gone: said so" "$OUT" "had already exited"
check "session-end, holder gone: another job's record left alone" "$(head -n 1 "$LOCK")" "btrdasd walk pid 777"
check "session-end, holder gone: its record removed" "$(ls -A "$STATE")" ""
stop_blocker

echo "--- the holder's scope (systemd-run)"
fixture
touch "$S/systemd_run_fail"
run_driver session A --dry-run
check "systemd-run fails: refused" "$RC" "1"
has "systemd-run fails: says why" "$OUT" "already exists"
check "systemd-run fails: lock released" "$(lock_state)" "free"
check "systemd-run fails: no record left" "$(ls -A "$STATE")" ""
lacks "systemd-run fails: no alarm about the lock" "$OUT" "STILL HELD"

fixture
touch "$S/systemd_run_orphan"
run_driver session A --dry-run
check "an orphan keeps the lock: refused" "$RC" "1"
has "an orphan keeps the lock: said loudly" "$OUT" "THE MAINTENANCE LOCK IS STILL HELD"
check "an orphan keeps the lock: it does" "$(lock_state)" "held"
kill -KILL "$(cat "$S/orphan.pid")" 2>/dev/null || :
for ((i = 0; i < 100; i++)); do [[ "$(lock_state)" == free ]] && break; sleep 0.05; done
check "an orphan keeps the lock: free once it is gone" "$(lock_state)" "free"

fixture
rm "$T/bin/systemd-run"
DRIVER_PATH="$(minimal_path)" run_driver session A --dry-run
check "no systemd-run: refused, no fallback" "$RC" "1"
has "no systemd-run: says so" "$OUT" "systemd-run is not available"
check "no systemd-run: no lock taken" "$(file "$S/flock.calls")" ""

# A runner that forks leaves its own pid in $!: the holder is the pid the
# held line names, and that is the process stopped.
fixture
touch "$S/systemd_run_forks"
run_driver session A --dry-run
check "a runner that forks: exit 0" "$RC" "0"
has "a runner that forks: the holder named by its line stopped" "$(events)" "holder released"
check "a runner that forks: lock released" "$(lock_state)" "free"
check "a runner that forks: no holder left running" "$(alive "$(head -n 1 "$S/holder.pids")")" "gone"

# The holder's scope is named after the label.
fixture
dump_env ZK208Q77 ZFL41DNY "recovery B 2tb"
write_state 3 "$(record_json system-recovery-A-2tb no)" "$(record_json "recovery B 2tb" no)"
run_driver session "recovery B 2tb" --dry-run
check "a label a unit name cannot carry: refused" "$RC" "1"
has "a label a unit name cannot carry: says so" "$OUT" "characters a systemd unit name cannot carry"
check "a label a unit name cannot carry: no lock taken" "$(file "$S/flock.calls")" ""

echo "--- the claim is lost, and cannot be taken again"
fixture
touch "$S/kill_holder_at_start" "$S/busy_after_start"
{
    for ((i = 0; i < 6; i++)); do echo running; done
    echo "shut off"
} >"$S/states.running"
run_driver session A
check "claim gap: exit 5" "$RC" "5"
has "claim gap: the summary says it was not taken again" "$OUT" "LOST 1 time(s); NOT claimed again"
has "claim gap: a warning line says what that means" "$OUT" "the claim was lost and not taken again"
lacks "claim gap: never said to be claimed again" "$OUT" "and claimed again"
check "claim gap: one loss, however many tries" "$(grep -c 'is gone while the recovery OS may use' <<<"$OUT")" "1"
check "claim gap: tried again at each check" "$(($(grep -c '^hold-disk' "$S/holder.calls") > 3))" "1"
check "claim gap: the lock's record names a running process at the last check" "$(tail -n 1 "$S/lock.records.alive")" "alive"
check "claim gap: lock free at the end" "$(lock_state)" "free"
has "claim gap: no holder said to be stopped" "$OUT" "detached; no holder left to stop;"

fixture
touch "$S/kill_holder_at_start" "$S/busy_after_start" "$S/busy_once"
{
    for ((i = 0; i < 6; i++)); do echo running; done
    echo "shut off"
} >"$S/states.running"
run_driver session A
check "claim gap closed: exit 5" "$RC" "5"
has "claim gap closed: claimed again" "$OUT" "LOST 1 time(s) and claimed again"
lacks "claim gap closed: no gap in the summary" "$OUT" "NOT claimed again"
check "claim gap closed: the record names the holder that closed it" "$(tail -n 1 "$S/lock.records")" \
    "recovery-os VM session system-recovery-A-2tb pid $(tail -n 1 "$S/holder.pids")"

echo "--- the drive re-enumerates during a session"
fixture
echo "$DISK_A ../../sdk" >"$S/reenumerate"
{
    for ((i = 0; i < 6; i++)); do echo running; done
    echo "shut off"
} >"$S/states.running"
run_driver session A
check "re-enumerated: exit 5" "$RC" "5"
has "re-enumerated: said so at once" "$OUT" "re-enumerated during the session"
has "re-enumerated: in the summary" "$OUT" "the claim was on the old device"
has "re-enumerated: the old node's mounts, labelled as such" "$OUT" "(old device $T/dev/sdj)"
has "re-enumerated: the current node checked too" "$OUT" "(current device $T/dev/sdk)"
check "re-enumerated: everything given back" "$(lock_state)" "free"

fixture
echo "$DISK_A ../../sdk" >"$S/reenumerate"
printf 'sdk \nsdk1 \nsdk2 /mnt/y\n' >"$S/lsblk.mounts.sdk"
{
    for ((i = 0; i < 6; i++)); do echo running; done
    echo "shut off"
} >"$S/states.running"
run_driver session A
check "re-enumerated, current node mounted: exit 5" "$RC" "5"
has "re-enumerated, current node mounted: said" "$OUT" "MOUNTED: sdk2 on /mnt/y (current device $T/dev/sdk)"

echo "--- a partition mounted when the session ends"
fixture
printf 'sdj \nsdj1 \nsdj2 /mnt/x\n' >"$S/lsblk.mounts.sdj.after_start"
run_driver session A
check "mounted after: exit 5" "$RC" "5"
has "mounted after: in the summary" "$OUT" "MOUNTED: sdj2 on /mnt/x"

echo "--- the disk cannot be given back, and the holder is gone"
fixture
mkdir -p "$STATE"
printf '%s\n%s\n' 999999999 "$DISK_A" >"$STATE/system-recovery-A-2tb.holder"
printf "<disk type='block' device='disk'>\n  <source dev='%s'/>\n</disk>\n" "$DISK_A" >"$S/attached.xml"
touch "$S/detach_fail"
run_driver session-end A
check "detach fails, holder gone: exit 4" "$RC" "4"
has "detach fails, holder gone: the claim is gone" "$OUT" "the claim is gone"
lacks "detach fails, holder gone: no dead holder named as claiming" "$OUT" "still claims"

echo "--- the boot record: would btrbk run when this OS boots (bd 1yg)"
fixture
run_driver session A --dry-run
check "record no: the session goes on" "$RC" "0"
has "record no: the verdict shown" "$OUT" "btrbk at boot  no"
has "record no: the enabled units shown" "$OUT" "enabled units  fstrim.timer, sshd.service (2)"
matches "record no: when it was checked, and how long ago" "$OUT" "checked        [0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2} UTC, 1h 00m ago"
has "record no: which record" "$OUT" "boot record for system-recovery-A-2tb ($STATE_FILE, schema 3)"

for v in will may; do
    fixture
    write_state 3 "$(record_json system-recovery-A-2tb "$v")" "$(record_json system-recovery-B-2tb no)"
    run_driver session A --dry-run
    check "record $v: refused" "$RC" "1"
    has "record $v: says so" "$OUT" "btrbk $v run when this OS boots"
    has "record $v: says how to put it right" "$OUT" "fix it from inside the recovery OS on bare metal, or check its config, then let the next backup run record it again"
    has "record $v: with its age" "$OUT" "1h 00m ago"
    has "record $v: how to boot it without running btrbk" "$OUT" "systemd.unit=emergency.target systemd.setenv=SYSTEMD_SULOGIN_FORCE=1 on its kernel line"
    has "record $v: then power off" "$OUT" "then systemctl poweroff"
    has "record $v: never leave the emergency shell" "$OUT" "never exit or Ctrl-D"
    has "record $v: never Ctrl-Alt-Del" "$OUT" "never Ctrl-Alt-Del"
    has "record $v: hold the power button at a prompt" "$OUT" "hold the power button"
    check "record $v: nothing locked" "$(file "$S/flock.calls")" ""
    check "record $v: nothing held" "$(file "$S/holder.calls")" ""
    check "record $v: nothing attached" "$(file "$S/attach.log")" ""
done
fixture
write_state 3 "$(record_json system-recovery-A-2tb will)"
run_driver session A --dry-run
has "record will: the reason shown" "$OUT" "  - btrbk.timer starts btrbk.service, which runs btrbk at its next scheduled time after boot"
matches "record will: the refusal itself says when the record was made" "$OUT" "REFUSED: btrbk will run when this OS boots \(the record was checked [0-9-]+ [0-9:]+ UTC, 1h 00m ago\)"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
run_driver session A --dry-run
has "record may: the reason shown" "$OUT" "  - not read"

fixture
rm "$STATE_FILE"
run_driver session A --dry-run
check "no record file: refused" "$RC" "1"
has "no record file: says so" "$OUT" "no boot record: $STATE_FILE does not exist"
check "no record file: nothing locked" "$(file "$S/flock.calls")" ""

fixture
rm "$STATE_FILE"
mkdir "$STATE_FILE"
run_driver session A --dry-run
check "record that cannot be read: refused" "$RC" "1"
has "record that cannot be read: says so" "$OUT" "the boot record $STATE_FILE cannot be read"

fixture
printf 'not json\n' >"$STATE_FILE"
run_driver session A --dry-run
check "record not JSON: refused" "$RC" "1"
has "record not JSON: says so" "$OUT" "the boot record $STATE_FILE cannot be read"

fixture
: >"$STATE_FILE"
run_driver session A --dry-run
check "record empty: refused" "$RC" "1"
has "record empty: says so" "$OUT" "not one JSON document"

fixture
printf '{"schema_version":3,"drives":{}}{"schema_version":3,"drives":{}}\n' >"$STATE_FILE"
run_driver session A --dry-run
check "two documents in the record: refused" "$RC" "1"
has "two documents: says so" "$OUT" "not one JSON document"

fixture
write_state 2 "$(record_json system-recovery-A-2tb no)"
run_driver session A --dry-run
check "schema 2: refused" "$RC" "1"
has "schema 2: says so" "$OUT" "is schema 2, not 3"
fixture
write_state 4 "$(record_json system-recovery-A-2tb no)"
run_driver session A --dry-run
check "schema 4, newer: refused too" "$RC" "1"
has "schema 4: says so" "$OUT" "is schema 4, not 3"

fixture
write_state 3 "$(record_json system-recovery-B-2tb no)"
run_driver session A --dry-run
check "no entry for the label: refused" "$RC" "1"
has "no entry for the label: says so" "$OUT" "has no entry for 'system-recovery-A-2tb'"

fixture
write_state 3 "\"system-recovery-A-2tb\":{\"checked_epoch\":$(($(date +%s) - 60)),\"os\":null,\"error\":\"cannot read /mnt/x/@: No such file or directory\"}"
run_driver session A --dry-run
check "no inspected OS: refused" "$RC" "1"
has "no inspected OS: says so, with the error" "$OUT" "has no inspected OS (cannot read /mnt/x/@: No such file or directory)"

fixture
write_state 3 "\"system-recovery-A-2tb\":{\"checked_epoch\":$(($(date +%s) - 60)),\"os\":{},\"error\":null}"
run_driver session A --dry-run
check "no verdict: refused" "$RC" "1"
has "no verdict: says so" "$OUT" "has no btrbk-at-boot verdict"

fixture
write_state 3 "\"system-recovery-A-2tb\":{\"checked_epoch\":$(($(date +%s) - 60)),\"os\":{\"btrbk_at_boot\":{\"verdict\":\"sometimes\",\"reasons\":[]}},\"error\":null}"
run_driver session A --dry-run
check "an unknown verdict: refused" "$RC" "1"
has "an unknown verdict: says so" "$OUT" "has no btrbk-at-boot verdict ('sometimes')"

fixture
write_state 3 "\"system-recovery-A-2tb\":{\"os\":{\"btrbk_at_boot\":{\"verdict\":\"no\",\"reasons\":[]}},\"error\":null}"
run_driver session A --dry-run
check "no check time: refused" "$RC" "1"
has "no check time: says so" "$OUT" "has no check time"

fixture
write_state 3 "$(record_json system-recovery-A-2tb no $((9 * 86400)))"
run_driver session A --dry-run
check "record 9 days old: refused" "$RC" "1"
has "record 9 days old: says so" "$OUT" "older than 8 days"
has "record 9 days old: with its age" "$OUT" "9d 0h ago"

fixture
write_state 3 "$(record_json system-recovery-A-2tb no 7200)"
run_driver session A --dry-run
check "record 2 hours old: the session goes on" "$RC" "0"

fixture
write_state 3 "$(record_json system-recovery-A-2tb no -7200)"
run_driver session A --dry-run
check "record from the future: refused" "$RC" "1"
has "record from the future: says so" "$OUT" "dated in the future"
has "record from the future: no negative age" "$OUT" "UTC, in the future)"

fixture
write_state 3 "$(record_json system-recovery-A-2tb no 7200)"
printf 'system-recovery-B-2tb %s\nsystem-recovery-A-2tb %s\n' "$(($(date +%s) - 60))" "$(($(date +%s) - 3600))" >"$SESSIONS"
run_driver session A --dry-run
check "record older than the last session: refused" "$RC" "1"
has "record older than the last session: says so" "$OUT" "before this drive's last VM session ended"

fixture
printf 'system-recovery-B-2tb %s\n' "$(($(date +%s) - 60))" >"$SESSIONS"
run_driver session A --dry-run
check "another drive's session does not count" "$RC" "0"

fixture
printf 'system-recovery-A-2tb %s\n' "$(($(date +%s) - 7200))" >"$SESSIONS"
run_driver session A --dry-run
check "a record made after the last session: the session goes on" "$RC" "0"

fixture
printf 'system-recovery-A-2tb yesterday\n' >"$SESSIONS"
run_driver session A --dry-run
check "a garbled session time: refused" "$RC" "1"
has "a garbled session time: says so" "$OUT" "cannot tell when this drive's last VM session ended"

fixture
: >"$SESSIONS"
run_driver session A --dry-run
check "an empty session-times file: refused" "$RC" "1"
has "an empty session-times file: says so" "$OUT" "$SESSIONS is empty"
run_driver session A --dry-run --accept-boot-record-risk
check "an empty session-times file: the override covers it" "$RC" "5"

fixture
printf 'system-recovery-B-2tb yesterday\nsystem-recovery-A-2tb %s\n' "$(($(date +%s) - 7200))" >"$SESSIONS"
run_driver session A --dry-run
check "another drive's garbled line: the file is not trusted" "$RC" "1"
has "another drive's garbled line: says which line" "$OUT" "line 1 of $SESSIONS is not"
has "another drive's garbled line: says how to put it right" "$OUT" "remove the bad line, or the whole file, at $SESSIONS, then run a backup with the drive attached"
lacks "another drive's garbled line: no advice that cannot help" "$OUT" "let the next backup run, with this drive attached, record it again"

fixture
printf 'system-recovery-A-2tb 0123\n' >"$SESSIONS"
run_driver session A --dry-run
check "a session time with a leading zero: refused" "$RC" "1"
has "a session time with a leading zero: says which line" "$OUT" "line 1 of $SESSIONS is not"

# Overridden, a session rewrites the file from its well-formed lines: another
# drive's line with no time it can read becomes that drive's line at "now"
# (so its next session waits for a new record), a line with no label goes.
fixture
printf 'system-recovery-B-2tb yesterday\nnot a session line at all\n' >"$SESSIONS"
run_driver session A --accept-boot-record-risk
check "overridden over a damaged file: ends, flagged" "$RC" "5"
matches "overridden over a damaged file: the other drive's line rewritten with a time" "$(grep '^system-recovery-B-2tb ' "$SESSIONS")" "^system-recovery-B-2tb [1-9][0-9]+$"
check "overridden over a damaged file: the line with no label dropped" "$(grep -c 'not a session line' "$SESSIONS")" "0"
has "overridden over a damaged file: says what it rewrote" "$OUT" "rewrote line 1 of $SESSIONS"
has "overridden over a damaged file: says what it dropped" "$OUT" "dropped line 2 of $SESSIONS"

fixture
mkdir -p "$SESSIONS"
run_driver session A --dry-run
check "session times not in a file: refused" "$RC" "1"
has "session times not in a file: says so" "$OUT" "is not a regular file"

echo "--- the boot record: --accept-boot-record-risk"
fixture
write_state 3 "$(record_json system-recovery-A-2tb will)"
run_driver session A --dry-run --accept-boot-record-risk
check "override, will: goes on, flagged (exit 5)" "$RC" "5"
has "override, will: the dry run says why it exits 5" "$OUT" "dry run: the boot-record check was overridden"
has "override, will: said loudly" "$OUT" "OVERRIDDEN (--accept-boot-record-risk): btrbk will run when this OS boots"

fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
run_driver session A --dry-run --accept-boot-record-risk
check "override, may: goes on, flagged (exit 5)" "$RC" "5"

fixture
write_state 3 "$(record_json system-recovery-A-2tb no $((9 * 86400)))"
run_driver session A --dry-run --accept-boot-record-risk
check "override, 9 days old: goes on, flagged (exit 5)" "$RC" "5"
has "override, 9 days old: said loudly" "$OUT" "OVERRIDDEN (--accept-boot-record-risk): the record is 9d 0h old"

fixture
write_state 3 "$(record_json system-recovery-A-2tb no 7200)"
printf 'system-recovery-A-2tb %s\n' "$(($(date +%s) - 3600))" >"$SESSIONS"
run_driver session A --dry-run --accept-boot-record-risk
check "override, before the last session: goes on, flagged (exit 5)" "$RC" "5"
has "override, before the last session: said loudly" "$OUT" "OVERRIDDEN (--accept-boot-record-risk): the record was made before this drive's last VM session ended"

fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
run_driver session A --accept-boot-record-risk
check "override, a real session: ends, flagged" "$RC" "5"
has "override, a real session: in the summary's warnings" "$OUT" "Warnings      the boot-record check was overridden (--accept-boot-record-risk): btrbk may run when this OS boots"

fixture
rm "$STATE_FILE"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers a missing record" "$RC" "1"
has "override never covers a missing record: that refusal, not another" "$OUT" "no boot record: $STATE_FILE does not exist"
fixture
printf 'not json\n' >"$STATE_FILE"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers a record that cannot be read" "$RC" "1"
has "override never covers a record that cannot be read: that refusal, not another" "$OUT" "the boot record $STATE_FILE cannot be read"
fixture
write_state 2 "$(record_json system-recovery-A-2tb no)"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers another schema" "$RC" "1"
has "override never covers another schema: that refusal, not another" "$OUT" "is schema 2, not 3"
fixture
write_state 3 "$(record_json system-recovery-B-2tb no)"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers a missing entry" "$RC" "1"
has "override never covers a missing entry: that refusal, not another" "$OUT" "has no entry for 'system-recovery-A-2tb'"
fixture
write_state 3 "\"system-recovery-A-2tb\":{\"checked_epoch\":$(($(date +%s) - 60)),\"os\":null,\"error\":\"x\"}"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers an OS not inspected" "$RC" "1"
has "override never covers an OS not inspected: that refusal, not another" "$OUT" "has no inspected OS (x)"
fixture
write_state 3 "\"system-recovery-A-2tb\":{\"checked_epoch\":$(($(date +%s) - 60)),\"os\":{},\"error\":null}"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers a missing verdict" "$RC" "1"
has "override never covers a missing verdict: that refusal, not another" "$OUT" "has no btrbk-at-boot verdict"
fixture
write_state 3 "\"system-recovery-A-2tb\":{\"os\":{\"btrbk_at_boot\":{\"verdict\":\"no\",\"reasons\":[]}},\"error\":null}"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers a missing check time" "$RC" "1"
has "override never covers a missing check time: that refusal, not another" "$OUT" "has no check time"
fixture
write_state 3 "\"system-recovery-A-2tb\":{\"checked_epoch\":$(($(date +%s) - 60)),\"os\":{\"btrbk_at_boot\":{\"verdict\":\"no\",\"reasons\":[]}},\"error\":\"\"}"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers an error without text" "$RC" "1"
has "an error without text: still an error" "$OUT" "has no inspected OS (an error without text)"
fixture
printf '{"schema_version":"3","drives":{%s}}\n' "$(record_json system-recovery-A-2tb no)" >"$STATE_FILE"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers a schema that is not a number" "$RC" "1"
has "a string schema: said as such" "$OUT" "schema_version is not the number 3"
lacks "a string schema: not misreported as a missing entry" "$OUT" "has no entry for"
fixture
write_state 3 "\"system-recovery-A-2tb\":{\"checked_epoch\":100000000000000000,\"os\":{\"btrbk_at_boot\":{\"verdict\":\"no\",\"reasons\":[]}},\"error\":null}"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers a check time date cannot read" "$RC" "1"
has "a check time date cannot read: a refusal that says so" "$OUT" "REFUSED: the boot record for 'system-recovery-A-2tb' has a check time this host cannot read as a date"
fixture
write_state 3 "\"system-recovery-A-2tb\":{\"checked_epoch\":$(($(date +%s) * 1000)),\"os\":{\"btrbk_at_boot\":{\"verdict\":\"no\",\"reasons\":[]}},\"error\":null}"
run_driver session A --dry-run --accept-boot-record-risk
check "override never covers a record dated more than a day ahead (milliseconds)" "$RC" "1"
has "a record dated more than a day ahead: said" "$OUT" "more than a day in the future"
fixture
write_state 3 "$(record_json system-recovery-A-2tb no -7200)"
run_driver session A --dry-run --accept-boot-record-risk
check "override, 2 hours ahead (a clock skew): goes on, flagged (exit 5)" "$RC" "5"
for t in '"015260430204"' '"089"' '"1791099438"' 0; do
    fixture
    write_state 3 "\"system-recovery-A-2tb\":{\"checked_epoch\":$t,\"os\":{\"btrbk_at_boot\":{\"verdict\":\"no\",\"reasons\":[]}},\"error\":null}"
    run_driver session A --dry-run --accept-boot-record-risk
    check "override never covers a check time of $t" "$RC" "1"
    has "a check time of $t: said, not misread" "$OUT" "REFUSED: the boot record for 'system-recovery-A-2tb' has no check time"
done

echo "--- the boot record: where it is read from"
fixture
OS_STATE="$T/elsewhere.json"
cp "$STATE_FILE" "$OS_STATE"
run_driver session A --dry-run
check "DAS_RECOVERY_OS_STATE without the test hatch: refused" "$RC" "1"
has "DAS_RECOVERY_OS_STATE without the test hatch: says why" "$OUT" "honoured only with the test hatch"
OS_STATE=""

fixture
rm "$T/bin/systemd-run"
ln -s "$(command -v bash)" "$T/bin/systemd-run"
DRIVER_PATH="$(minimal_path)" run_driver session A --dry-run
check "no jq: refused" "$RC" "1"
has "no jq: says so" "$OUT" "jq is not installed"
check "no jq: no lock taken" "$(file "$S/flock.calls")" ""

fixture
OS_STATE="$T/elsewhere.json"
cp "$STATE_FILE" "$OS_STATE"
mkdir -p "$STATE"
printf '%s\n%s\n' 999999999 "$DISK_A" >"$STATE/system-recovery-A-2tb.holder"
run_driver session-end A
check "session-end, DAS_RECOVERY_OS_STATE without the hatch: refused" "$RC" "1"
has "session-end, DAS_RECOVERY_OS_STATE without the hatch: says why" "$OUT" "honoured only with the test hatch"
check "session-end, DAS_RECOVERY_OS_STATE without the hatch: nothing done" "$(file "$S/virsh.calls")" ""
check "session-end, DAS_RECOVERY_OS_STATE without the hatch: no session time written" "$(file "$T/recovery-os-vm-sessions")" ""
OS_STATE=""

fixture
LOOP="$T/dev/loop7"
run_driver session-end system-recovery-A-2tb
check "session-end under the hatch without DAS_RECOVERY_OS_STATE: refused" "$RC" "1"
has "session-end under the hatch without DAS_RECOVERY_OS_STATE: says why" "$OUT" "the test hatch needs DAS_RECOVERY_OS_STATE"
LOOP=""

echo "--- the time of each session, kept for the boot-record check"
fixture
run_driver session A --dry-run
check "a dry run: no session time recorded" "$(file "$SESSIONS")" ""
fixture
printf 'system-recovery-B-2tb 1000\n' >"$SESSIONS"
run_driver session A
check "session: exit 0" "$RC" "0"
matches "session: its time recorded" "$(grep '^system-recovery-A-2tb ' "$SESSIONS")" "^system-recovery-A-2tb [0-9]+$"
check "session: the other drives' times kept" "$(grep '^system-recovery-B-2tb ' "$SESSIONS")" "system-recovery-B-2tb 1000"
check "session: one line per drive" "$(wc -l <"$SESSIONS")" "2"
matches "session: the new times flushed before they replace the old" "$(head -n 1 "$S/sync.calls")" "^-- .*/recovery-os-vm-sessions\.new\.[0-9]+$"
check "session: and the directory after, for the start and the end" "$(grep -cx -- "-- $T/var/lib/das-backup" "$S/sync.calls")" "2"
matches "session: recorded before the OS booted" "$(grep '^system-recovery-A-2tb ' "$S/sessions.at_start")" "^system-recovery-A-2tb [0-9]+$"
check "session: readable by all, written by its owner only" "$(stat -c %a "$SESSIONS")" "644"
run_driver session A --dry-run
check "the next session: refused until a backup run checks the OS again" "$RC" "1"
has "the next session: says why" "$OUT" "before this drive's last VM session ended"
write_state 3 "$(record_json system-recovery-A-2tb no -1)" "$(record_json system-recovery-B-2tb no)"
run_driver session A --dry-run
check "the next session, after a new check: goes on" "$RC" "0"
run_driver session B --dry-run
check "the other drive: not affected" "$RC" "0"

fixture
mkdir -p "$SESSIONS"
run_driver session A --accept-boot-record-risk
check "session time cannot be recorded: refused" "$RC" "1"
has "session time cannot be recorded: says why" "$OUT" "cannot record the start of this session"
lacks "session time cannot be recorded: the OS never booted" "$(events)" "virsh start"
check "session time cannot be recorded: lock free" "$(lock_state)" "free"

fixture
touch "$S/start_fail"
run_driver session A
check "start failed: refused" "$RC" "1"
matches "start failed: counted as a session all the same (it may have half-started)" "$(file "$SESSIONS")" "^system-recovery-A-2tb [0-9]+$"

echo "--- the test hatch (DAS_RECOVERY_VM_TEST_LOOP)"
fixture
: >"$T/dev/sdz"
echo "block special file:8" >"$S/stat.sdz"
LOOP="$T/dev/sdz"
hatch_state no
run_driver session system-recovery-A-2tb --dry-run
check "hatch: a non-loop device refused" "$RC" "1"
has "hatch: says why" "$OUT" "is not a loop device"
check "hatch: non-loop never held" "$(file "$S/holder.calls")" ""

fixture
: >"$T/dev/loop7"
: >"$T/dev/loop7p2"
echo "block special file:7" >"$S/stat.loop7"
echo loop >"$S/lsblk.type.loop7"
mkdir -p "$T/sys/block/loop7/loop"
: >"$T/rv.img"
LOOP="$T/dev/loop7"
hatch_state no
echo /dev/null >"$T/sys/block/loop7/loop/backing_file"
run_driver session system-recovery-A-2tb --dry-run
check "hatch: a loop backed by a device refused" "$RC" "1"
has "hatch: says what backs it" "$OUT" "backed by /dev/null, not a regular file"
rm "$T/sys/block/loop7/loop/backing_file"
run_driver session system-recovery-A-2tb --dry-run
check "hatch: a loop with no backing file refused" "$RC" "1"
has "hatch: says it has none" "$OUT" "has no backing file"
echo "$T/rv.img" >"$T/sys/block/loop7/loop/backing_file"
run_driver session A --dry-run
check "hatch: a shorthand refused" "$RC" "1"
has "hatch: wants the full label" "$OUT" "needs the target's full label"
hatch_state no primary-22tb
run_driver session primary-22tb --dry-run
check "hatch: still never a primary target" "$RC" "1"
hatch_state no
echo active >"$S/unit.das-backup.service"
run_driver session system-recovery-A-2tb --dry-run
check "hatch: the other checks still apply" "$RC" "1"
rm "$S/unit.das-backup.service"
# The hatch reads its own record, and nothing else: the usual one says will.
write_state 3 "$(record_json system-recovery-A-2tb will)"
hatch_state no
run_driver session system-recovery-A-2tb --dry-run
check "hatch: DAS_RECOVERY_OS_STATE honoured" "$RC" "0"
has "hatch: says which record it read" "$OUT" "boot record for system-recovery-A-2tb ($T/hatch/recovery-os.json, schema 3)"
OS_STATE=""
run_driver session system-recovery-A-2tb --dry-run
check "hatch: without its own record, refused" "$RC" "1"
has "hatch: without its own record, says why" "$OUT" "the test hatch needs DAS_RECOVERY_OS_STATE"
hatch_state no
run_driver session system-recovery-A-2tb
check "hatch: a loop device lent" "$RC" "0"
check "hatch: its session time kept beside its own record" "$(grep -c '^system-recovery-A-2tb ' "$T/hatch/recovery-os-vm-sessions")" "1"
check "hatch: the usual session times untouched" "$(file "$SESSIONS")" ""
has "hatch: announced" "$OUT" "TEST HATCH"
has "hatch: the loop device attached" "$(file "$S/attach.log")" "<source dev='$LOOP'/>"
check "hatch: loop partition 2 rescanned" "$(file "$S/btrfs.calls")" "device scan ${LOOP}p2"
LOOP=""
OS_STATE=""

echo "--- define"
fixture
rm "$S/defined"
run_driver define
check "define new: exit 0" "$RC" "0"
has "define new: validated against the schema" "$(file "$S/virsh.calls")" "define --validate $T/usr/lib/das-backup/libvirt/recovery-os-updater.xml"
nv="$T/var/lib/libvirt/qemu/nvram/recovery-os-updater_VARS.fd"
check "define new: the NVRAM is libvirt's to create" "$(ls -A "$T/var/lib/libvirt/qemu/nvram" 2>/dev/null)" ""
mkdir -p "$(dirname "$nv")"
printf 'BOOT-ENTRIES' >"$nv"
run_driver define
check "define again: exit 0" "$RC" "0"
check "define again: NVRAM untouched" "$(file "$nv")" "BOOT-ENTRIES"

# The domain on a host that defined it from a template without per-device
# boot: libvirt stored an os-level boot device in it.
fixture
printf "  <os>\n    <type arch='x86_64' machine='q35'>hvm</type>\n    <boot dev='hd'/>\n  </os>\n" >"$S/stored-os.xml"
run_driver define
check "define over a stored os-level boot: exit 0" "$RC" "0"
has "define over a stored os-level boot: says it updates" "$OUT" "updating recovery-os-updater from $T/usr/lib/das-backup/libvirt/recovery-os-updater.xml"
has "define over a stored os-level boot: redefined from the template" "$(file "$S/virsh.calls")" \
    "define --validate $T/usr/lib/das-backup/libvirt/recovery-os-updater.xml"
lacks "define over a stored os-level boot: what libvirt is handed has none" "$(file "$S/defined.xml")" "<boot dev="
has "define over a stored os-level boot: what libvirt is handed boots per device" "$(file "$S/defined.xml")" "<boot order='2'/>"

fixture
printf 'running\n' >"$S/states"
run_driver define
check "define while running: refused" "$RC" "1"
lacks "define while running: not redefined" "$(file "$S/virsh.calls")" "define --validate"

fixture
printf "<disk type='block' device='disk'>\n  <source dev='%s'/>\n</disk>\n" "$DISK_A" >"$S/attached.xml"
run_driver define
check "define with a disk attached: refused" "$RC" "1"
lacks "define with a disk attached: not redefined" "$(file "$S/virsh.calls")" "define --validate"

fixture
rm "$T/usr/share/edk2/x64/OVMF_CODE.4m.fd"
run_driver define
check "define without the firmware: refused" "$RC" "1"
has "define without the firmware: names it" "$OUT" "/usr/share/edk2/x64/OVMF_CODE.4m.fd is not installed"

echo "--- screenshot"
fixture
run_driver screenshot "$T/shot.png"
check "screenshot while shut off: refused" "$RC" "1"
printf 'running\n' >"$S/states"
run_driver screenshot "$T/shot.png"
check "screenshot while running: exit 0" "$RC" "0"
has "screenshot: converted to PNG" "$(file "$S/magick.calls")" "png:$T/shot.png"
check "screenshot: written" "$(file "$T/shot.png")" "PNG"

echo
echo "$passes passed, $fails failed"
if ((fails > 0)); then
    exit 1
fi
echo "RECOVERY-OS-VM SUITE GREEN"
