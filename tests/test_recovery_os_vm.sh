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
REAL_STAT="$(command -v stat)"
export REAL_STAT

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
        # The reset watch's virsh, if a driver left it.
        if [[ -f "$root/stub/event.pids" ]]; then
            while read -r pid; do
                if [[ "$(tr '\0' ' ' 2>/dev/null <"/proc/$pid/cmdline")" == *" event "* ]]; then
                    kill -KILL "$pid" 2>/dev/null || :
                fi
            done <"$root/stub/event.pids"
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

# Every `virsh destroy` a driver runs, in any test of this suite: one line
# each, "after a resume" or "never resumed". The suite ends by asserting the
# first kind never happened -- a domain that was resumed may have run, and
# only ACPI may stop it (I-1) -- across every scenario, not only the tests
# that look for it.
DESTROY_LOG="$(mktemp)"
ROOTS+=("$DESTROY_LOG")
export DESTROY_LOG

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
    H="$T/var/lib/das-backup/recovery-os-vm-history.jsonl"
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
    # Drive B's domain, defined and shut off, with a state of its own.
    mkdir -p "$S/domB"
    touch "$S/domB/defined"
    printf 'shut off\n' >"$S/domB/states"
    printf 'running\nrunning\nrunning\nshut off\n' >"$S/domB/states.running"
    : >"$S/domB/events"
    : >"$S/events"
    dump_env ZK208Q77 ZFL41DNY
    write_state 3 "$(record_json system-recovery-A-2tb no)" "$(record_json system-recovery-B-2tb no)"
    write_stubs
}

# The mount_uuid config gives a label now (the dump-env file): the filesystem
# a record made of that drive names. Empty for a label config does not have.
label_uuid() {
    local i
    for ((i = 0; i < 3; i++)); do
        if grep -qxF "DAS_TARGET_${i}_LABEL='$1'" "$S/dump-env"; then
            sed -n "s/^DAS_TARGET_${i}_MOUNT_UUID='\(.*\)'\$/\1/p" "$S/dump-env"
            return
        fi
    done
}

# The "mount_uuid": field of a record (bd DAS-Backup-Manager-df0): $1 the
# label, $2 empty for the filesystem config names for it, ABSENT for no field
# at all (a record from before btrdasd kept one), else the raw JSON value.
uuid_field() {
    case "$2" in
        "") printf '"mount_uuid":"%s",' "$(label_uuid "$1")" ;;
        ABSENT) ;;
        *) printf '"mount_uuid":%s,' "$2" ;;
    esac
}

# One drive's entry in the boot record btrdasd keeps (`recovery-os status
# --state-file`, schema 3), in the shape bd DAS-Backup-Manager-1yg defines,
# with the filesystem it was read from (df0): $1 label, $2 verdict (no, will,
# may -- or anything, to test), $3 seconds since it was checked (an hour if
# not given), $4 its mount_uuid as uuid_field takes it.
record_json() {
    local reasons='[]'
    case "$2" in
        will) reasons='["btrbk.timer starts btrbk.service, which runs btrbk at its next scheduled time after boot, with /etc/btrbk/btrbk.conf present"]' ;;
        may) reasons='["not read"]' ;;
    esac
    printf '"%s":{"checked_epoch":%s,%s"os":{"btrbk_at_boot":{"verdict":"%s","reasons":%s,"runners":[]},"enabled_units":{"state":"listed","units":[{"name":"fstrim.timer","dirs":["etc/systemd/system/timers.target.wants"]},{"name":"sshd.service","dirs":["etc/systemd/system/multi-user.target.wants"]}]},"btrbk_config":{"state":"absent"}},"error":null}' \
        "$1" "$(($(date +%s) - ${3:-3600}))" "$(uuid_field "$1" "${4:-}")" "$2" "$reasons"
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

# A drive's entry with reasons, runners and more enabled units: $1 label,
# $2 verdict, $3 the reasons and $4 the runners (JSON arrays), then the names
# of more enabled units.
record_with_runners() {
    local label=$1 verdict=$2 reasons=$3 runners=$4 units='{"name":"fstrim.timer","dirs":["etc/systemd/system/timers.target.wants"]}' u
    shift 4
    for u in "$@"; do units+=",{\"name\":\"$u\",\"dirs\":[\"etc/systemd/system/multi-user.target.wants\"]}"; done
    printf '"%s":{"checked_epoch":%s,%s"os":{"btrbk_at_boot":{"verdict":"%s","reasons":%s,"runners":%s},"enabled_units":{"state":"listed","units":[%s]},"btrbk_config":{"state":"absent"}},"error":null}' \
        "$label" "$(($(date +%s) - 3600))" "$(uuid_field "$label" "")" "$verdict" "$reasons" "$runners" "$units"
}

# One credential of a domain definition, decoded: $1 the file, $2 its name
# (a binary credential, base64 in the definition). Empty when there is none:
# the checks on it then fail one by one, and the suite goes on.
credential() {
    local v
    v=$(sed -n "s|.*<entry>io\.systemd\.credential\.binary:$2=\([^<]*\)</entry>.*|\1|p" "$1" 2>/dev/null | head -n 1) || v=""
    printf '%s' "$v" | base64 -d 2>/dev/null || :
}

# Drive $1's (A or B) domain definition as the driver must render it from the
# template: its own name, title, NVRAM file and the identity derived from its
# label -- pinned here, so a change to the derivation (a new UUID or MAC, a new
# network card to that recovery OS) cannot pass unnoticed.
rendered() {
    local uuid mac
    case "$1" in
        A) uuid=902e4129-0bcb-84a9-a10b-36aa12a6f88d mac=52:54:00:fa:7e:aa ;;
        B) uuid=d92ed93a-82ba-824a-a786-a30dc1fe4cf3 mac=52:54:00:26:e4:6a ;;
    esac
    sed -e "s|^  <name>recovery-os-updater</name>$|  <name>recovery-os-updater-system-recovery-$1-2tb</name>|" \
        -e "s|^  <uuid>.*</uuid>$|  <uuid>$uuid</uuid>|" \
        -e "s|^  <title>DAS recovery OS updater</title>$|  <title>DAS recovery OS updater: system-recovery-$1-2tb</title>|" \
        -e "s|>/var/lib/libvirt/qemu/nvram/recovery-os-updater_VARS.fd</nvram>|>/var/lib/libvirt/qemu/nvram/recovery-os-updater-system-recovery-$1-2tb_VARS.fd</nvram>|" \
        -e "s|<mac address='52:54:00:8a:60:e1'/>|<mac address='$mac'/>|" \
        "$T/usr/lib/das-backup/libvirt/recovery-os-updater.xml"
}

# yes when the virsh calls matching each fixed string came in this order.
calls_in_order() {
    local p n prev=0
    for p in "$@"; do
        n=$(grep -n -m1 -F -- "$p" "$S/virsh.calls" | cut -d: -f1)
        if [[ -z "$n" ]] || ((n <= prev)); then echo no; return; fi
        prev=$n
    done
    echo yes
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
BOOT_A=aaaaaaaa-1111-4222-8333-444444444444
if [[ "${1:-}" == --connect ]]; then shift 2; fi
printf '%s\n' "$*" >>"$S/virsh.calls"
printf '%s\n' "${LC_ALL-unset}" >>"$S/virsh.lc_all"
cmd=${1:-}
shift || :
# Which domain: one domain per recovery drive. Drive B's has a state of its
# own when the test gives it one (stub/domB); the shared domain of before
# (recovery-os-updater) when a test defines it (stub/legacy).
DOMNAME=""
for a in "$@"; do
    case "$a" in recovery-os-updater*) DOMNAME=$a; break ;; esac
done
if [[ "$cmd" == define ]]; then DOMNAME=$(sed -n 's|^ *<name>\(.*\)</name>$|\1|p' "${!#}" | head -n 1); fi
case "$DOMNAME" in
    recovery-os-updater-system-recovery-B-2tb) [[ ! -d "$S/domB" ]] || S="$S/domB" ;;
    recovery-os-updater) S="$S/legacy"; mkdir -p "$S"; [[ -f "$S/states" ]] || printf 'shut off\n' >"$S/states" ;;
    recovery-os-updater-*) ;;
    "") ;;
    *) echo "UNEXPECTED domain $DOMNAME" >>"$STUB/forbidden"; exit 99 ;;
esac
nodomain() { echo "error: failed to get domain '$DOMNAME'" >&2; exit 1; }
# The guest's reporter, modelled: one line as das-vm-guard-report writes it --
# in full when its state changed (or it started), else the masks' digest.
guest_line() {
    local boot seq mode last body
    boot=$(cat "$S/guest.boot") seq=$(cat "$S/guest.seq") mode=$(cat "$S/guest.mode")
    last=$(cat "$S/guest.last" 2>/dev/null || :)
    if [[ "$mode" == "$last" ]]; then body=$(cat "$S/guest.short_$mode"); else body=$(cat "$S/guest.full_$mode"); fi
    printf '%s seq %s boot %s\n' "$body" "$((seq + 1))" "$boot"
}
guest_sent() { # the line guest_line made was written
    echo "$(($(cat "$S/guest.seq") + 1))" >"$S/guest.seq"
    cat "$S/guest.mode" >"$S/guest.last"
}
# virtlogd's rotation: the file to .0, .0 to .1, ..., a new empty file.
guest_rotate() {
    local port k
    port=$(cat "$S/guard_port")
    for ((k = 8; k >= 0; k--)); do [[ ! -e "$port.$k" ]] || mv "$port.$k" "$port.$((k + 1))"; done
    mv "$port" "$port.0"
    : >"$port"
}
# The resets this domain's watch recorded (every drive's, for no domain).
resets_seen() {
    if [[ -n "$DOMNAME" ]]; then
        grep -c '^reset ' "$DAS_RECOVERY_VM_TEST_ROOT/run/das-recovery-os-vm/${DOMNAME#recovery-os-updater-}.resets" 2>/dev/null || :
    else
        cat "$DAS_RECOVERY_VM_TEST_ROOT"/run/das-recovery-os-vm/*.resets 2>/dev/null | grep -c '^reset ' || :
    fi
}
# One look at the running guest (poll $1): what guest.script says happens at
# this poll, then the reporter's line -- unless silent.
guest_step() {
    local p act arg port skip=false n i line
    port=$(cat "$S/guard_port")
    if [[ -f "$S/guest.script" ]]; then
        while read -r p act arg; do
            [[ "$p" == "$1" ]] || continue
            case "$act" in
                silence) touch "$S/guest.silent" ;;
                talk) rm -f "$S/guest.silent" ;;
                lift) echo lifted >"$S/guest.mode" ;;
                engage) echo engaged >"$S/guest.mode" ;;
                reset)
                    # A reset: libvirt's event first, seen by the driver's watch
                    # before the new boot says anything (it never could sooner).
                    n=$(resets_seen)
                    echo reset >>"$S/reboots.pending"
                    if [[ ! -f "$S/event_dies" ]]; then
                        for ((i = 0; i < 300; i++)); do (($(resets_seen) > n)) && break; sleep 0.01; done
                    fi
                    echo "$arg" >"$S/guest.boot"
                    echo 0 >"$S/guest.seq"
                    echo engaged >"$S/guest.mode"
                    : >"$S/guest.last"
                    skip=true
                    ;;
                boot)
                    # A new boot with no reset (kexec): its first line is $arg's.
                    echo "${arg%% *}" >"$S/guest.boot"
                    i=${arg#* }
                    [[ "$i" != "$arg" ]] || i=1
                    echo "$((i - 1))" >"$S/guest.seq"
                    : >"$S/guest.last"
                    ;;
                notengaged)
                    printf 'das-vm-guard NOT engaged: btrbk.timer is not masked (loaded); seq %s boot %s\n' \
                        "$(($(cat "$S/guest.seq") + 1))" "$(cat "$S/guest.boot")" >>"$port"
                    echo "$(($(cat "$S/guest.seq") + 1))" >"$S/guest.seq"
                    : >"$S/guest.last"
                    skip=true
                    ;;
                rotate) guest_rotate ;;
                say)
                    # One line now, before what follows in this same look.
                    guest_line >>"$port"
                    guest_sent
                    ;;
                drop)
                    # virtlogd past max_backups: the oldest rotated file deleted.
                    for ((i = 9; i >= 0; i--)); do
                        if [[ -e "$port.$i" ]]; then rm -f "$port.$i"; break; fi
                    done
                    ;;
                rotate-split)
                    # A line cut in two by the rotation.
                    line=$(guest_line)
                    printf '%s' "${line:0:20}" >>"$port"
                    guest_rotate
                    printf '%s\n' "${line:20}" >>"$port"
                    guest_sent
                    skip=true
                    ;;
                truncate) : >"$port" ;;
                # A line of the reset watch's record that cannot be parsed.
                badreset) echo "reset ?" >>"$(echo "$DAS_RECOVERY_VM_TEST_ROOT"/run/das-recovery-os-vm/*.resets)" ;;
            esac
        done <"$S/guest.script"
    fi
    if [[ "$skip" != true && ! -f "$S/guest.silent" ]]; then
        guest_line >>"$port"
        guest_sent
    fi
}
case "$cmd" in
    dominfo)
        [[ -f "$S/defined" ]] || nodomain
        echo "Name:           $DOMNAME"
        # Whether a managed-save image is there: told, or one appearing once
        # the disk is attached (between the preflight and the start).
        ms=no
        if [[ -f "$S/managed_save" ]]; then ms=$(cat "$S/managed_save"); fi
        if [[ -f "$S/managed_save_at_attach" && -f "$S/attached.xml" ]]; then ms=yes; fi
        [[ "$ms" == none ]] || echo "Managed save:   $ms"
        ;;
    list)
        [[ "$*" == "--all --name" ]] || { echo "UNEXPECTED list $*" >>"$STUB/forbidden"; exit 99; }
        if [[ -f "$S/list_fail" ]]; then echo "error: failed to connect to the hypervisor" >&2; exit 1; fi
        [[ ! -f "$S/defined" ]] || echo recovery-os-updater-system-recovery-A-2tb
        if [[ -d "$S/domB" ]]; then
            [[ ! -f "$S/domB/defined" ]] || echo recovery-os-updater-system-recovery-B-2tb
        else
            [[ ! -f "$S/defined" ]] || echo recovery-os-updater-system-recovery-B-2tb
        fi
        [[ ! -f "$S/legacy/defined" ]] || echo recovery-os-updater
        echo
        ;;
    event)
        # libvirt's event stream, as the reset watch reads it: one line per
        # reset (a line in reboots.pending), until it is ended -- or dies.
        [[ "$*" == "--domain $DOMNAME --event reboot --loop" && "$DOMNAME" == recovery-os-updater-* ]] || { echo "UNEXPECTED event $*" >>"$STUB/forbidden"; exit 99; }
        echo $$ >>"$STUB/event.pids"
        # The driver starts this itself, beside the reset watch that reads it:
        # whether the driver still runs when this ends says whether the
        # driver ended it on its way out.
        driver=$PPID
        echo "started" >>"$S/event.log"
        # Slow to end on a signal (event_slow_end), as on a loaded host: the
        # driver must wait for it, not kill it -- killed, it can say nothing.
        # Or deaf to SIGTERM (event_ignores_term): only a SIGKILL ends it.
        # It ends once: a second signal while it ends (a Ctrl-C reaches it
        # beside the watch's SIGTERM) is not a second end.
        slow=0
        [[ ! -f "$S/event_slow_end" ]] || slow=1
        trap 'trap "" TERM INT HUP; ((slow == 0)) || sleep 1; if [[ -d /proc/$driver ]] && ! grep -q "^State:[[:space:]]*Z" "/proc/$driver/status" 2>/dev/null; then echo "ended while the driver ran" >>"$S/event.log"; else echo "ended after the driver" >>"$S/event.log"; fi; exit 0' TERM INT HUP
        [[ ! -f "$S/event_ignores_term" ]] || trap '' TERM
        # A SIGTERM to the driver the moment this exists (event_terms_driver):
        # as close to this virsh's start as an interrupt can come from here.
        [[ ! -f "$S/event_terms_driver" ]] || kill -TERM "$driver"
        if [[ -f "$S/event_dies" ]]; then echo "error: internal error: client socket is closed" >&2; exit 1; fi
        n=0
        while :; do
            m=$(cat "$S/reboots.pending" 2>/dev/null | wc -l)
            while ((n < m)); do
                if [[ -f "$S/event_garbled" ]]; then
                    # Output the watch does not recognise, that names a reboot.
                    echo "event 'reboot' for domain: '$DOMNAME'"
                else
                    echo "event 'reboot' for domain '$DOMNAME'"
                fi
                n=$((n + 1))
            done
            sleep 0.02 &
            wait $!
        done
        ;;
    domstats)
        # One read of the state and the vCPUs' time (libvirt: utime + stime of
        # each vCPU thread): 0 until the guest has run.
        [[ "$*" == "--state --vcpu $DOMNAME" && "$DOMNAME" == recovery-os-updater-* ]] || { echo "UNEXPECTED domstats $*" >>"$STUB/forbidden"; exit 99; }
        [[ -f "$S/defined" ]] || nodomain
        if [[ -f "$S/domstats_fail" ]]; then echo "error: failed to connect to the hypervisor" >&2; exit 1; fi
        state=$(head -n 1 "$S/states")
        case "$state" in
            running) n=1 ;;
            paused) n=3 ;;
            "in shutdown") n=4 ;;
            "shut off") n=5 ;;
            *) n=0 ;;
        esac
        printf "Domain: '%s'\n  state.state=%s\n  state.reason=1\n" "$DOMNAME" "$n"
        if [[ "$state" != "shut off" ]]; then
            printf '  vcpu.current=4\n  vcpu.maximum=4\n'
            # libvirt leaves the per-vCPU fields out when it cannot read them.
            if [[ ! -f "$S/domstats_no_vcpu" ]]; then
                for i in 0 1 2 3; do
                    t=0
                    if [[ -f "$S/resumed" ]] || [[ -f "$S/vcpu_ran" && "$i" == 2 ]]; then t=1530000000; fi
                    printf '  vcpu.%s.state=1\n  vcpu.%s.time=%s\n  vcpu.%s.wait=0\n' "$i" "$i" "$t" "$i"
                done
            fi
        fi
        echo
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
            if [[ -f "$S/guest.boot" && "$(head -n 1 "$S/states")" == running ]]; then
                guest_step "$polls"
            fi
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
        if [[ "${1:-}" != --inactive && -f "$S/dumpxml_live_fail" && -f "$S/started" ]]; then
            echo "error: internal error: client socket is closed" >&2
            exit 1
        fi
        # Something other than the driver resumes the domain right here.
        if [[ "${1:-}" != --inactive && -f "$S/resume_at_live_dumpxml" && -f "$S/started" ]]; then
            cp "$S/states.running" "$S/states"
            touch "$S/resumed_by_other"
        fi
        # The read takes long enough for an interrupt to land while paused.
        if [[ "${1:-}" != --inactive && -f "$S/dumpxml_live_sleep" && -f "$S/started" ]]; then
            touch "$S/in_live_dumpxml"
            sleep 10
        fi
        if [[ -f "$S/defined.xml" ]]; then
            # What was defined last, with the disk attached since in it. A
            # running domain that booted without the guard, when told so.
            while IFS= read -r line || [[ -n "$line" ]]; do
                if [[ "$line" == "  </devices>" && -f "$S/attached.xml" ]]; then sed 's/^/    /' "$S/attached.xml"; fi
                if [[ "${1:-}" != --inactive && -f "$S/live_drops_guard" && -f "$S/started" && "$line" == *"<entry>"* ]]; then continue; fi
                printf '%s\n' "$line"
            done <"$S/defined.xml"
        else
            printf "<domain type='kvm' id='1'>\n  <name>%s</name>\n" "$DOMNAME"
            if [[ -f "$S/stored-os.xml" ]]; then cat "$S/stored-os.xml"; fi
            printf "  <devices>\n"
            if [[ -f "$S/attached.xml" ]]; then sed 's/^/    /' "$S/attached.xml"; fi
            printf "  </devices>\n</domain>\n"
        fi
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
        # What the reset watch recorded, before giving the disk back takes it away.
        cat "$DAS_RECOVERY_VM_TEST_ROOT"/run/das-recovery-os-vm/*.resets >"$S/resets.at_detach" 2>/dev/null || :
        echo "Disk detached successfully"
        ;;
    start)
        echo "virsh start" >>"$S/events"
        head -n 1 "$DAS_RECOVERY_VM_TEST_ROOT/run/das-maintenance.lock" >"$S/lock.record.at_start"
        cat "$DAS_RECOVERY_VM_TEST_ROOT/var/lib/das-backup/recovery-os-vm-sessions" >"$S/sessions.at_start" 2>/dev/null || :
        if [[ -f "$S/start_fail" ]]; then echo "error: Failed to start domain 'recovery-os-updater'" >&2; exit 1; fi
        [[ " $* " == *" --paused "* ]] || { echo "stub: start without --paused" >&2; echo "UNPAUSED START" >>"$STUB/forbidden"; exit 98; }
        # Started paused: the guest has executed nothing yet.
        printf 'paused\n' >"$S/states"
        if [[ -f "$S/states.after_start" ]]; then cp "$S/states.after_start" "$S/states"; fi
        touch "$S/started"
        cp "$S/defined.xml" "$S/defined.at_start.xml" 2>/dev/null || :
        # A start that says it failed although the domain did start.
        if [[ -f "$S/start_error_but_started" ]]; then echo "error: Timed out during operation: cannot acquire state change lock" >&2; exit 1; fi
        echo "Domain 'recovery-os-updater' started"
        ;;
    resume)
        echo "virsh resume" >>"$S/events"
        cat "$DAS_RECOVERY_VM_TEST_ROOT/var/lib/das-backup/recovery-os-vm-sessions" >"$S/sessions.at_resume" 2>/dev/null || :
        cat "${DAS_RECOVERY_OS_STATE:-/nonexistent}" >/dev/null 2>&1 || :
        touch "$S/resumed"
        # From here on the domain's state cannot be read.
        if [[ -f "$S/domstate_fail_after_resume" ]]; then touch "$S/domstate_fail"; fi
        if [[ -f "$S/resume_fail" ]]; then echo "error: Failed to resume domain 'recovery-os-updater'" >&2; exit 1; fi
        cp "$S/states.running" "$S/states"
        # The guest runs: a healthy one's guard reports every mask it was given,
        # then a heartbeat at every look -- unless told otherwise.
        if grep -q "name='org.dasbackup.guard'" "$S/defined.xml" 2>/dev/null && [[ ! -f "$S/guard_silent" ]]; then
            port=$(sed -n "s|.*<source path='\([^']*\)'/>.*|\1|p" "$S/defined.xml" | head -n 1)
            printf '%s\n' "$port" >"$S/guard_port"
            if [[ -f "$S/guard_reply" ]]; then
                cp "$S/guard_reply" "$port"
            else
                masks=$(grep -o 'io\.systemd\.credential:systemd\.extra-unit\.[^=<]*=' "$S/defined.xml" | sed 's/^io\.systemd\.credential:systemd\.extra-unit\.//; s/=$//')
                n=$(grep -c . <<<"$masks")
                list=$(paste -sd, - <<<"$masks")
                digest=$(printf '%s' "$list" | sha256sum | cut -c1-16)
                for mode in engaged lifted; do
                    printf 'das-vm-guard %s %s masks %s\n' "$mode" "$n" "$list" >"$S/guest.full_$mode"
                    printf 'das-vm-guard %s %s masks sha256:%s\n' "$mode" "$n" "$digest" >"$S/guest.short_$mode"
                done
                echo "$BOOT_A" >"$S/guest.boot"
                echo 0 >"$S/guest.seq"
                echo engaged >"$S/guest.mode"
                : >"$S/guest.last"
                : >"$port"
                # Poll 0 is the resume: a guest silent from its start says so there.
                if grep -qx '0 silence' "$S/guest.script" 2>/dev/null; then touch "$S/guest.silent"; fi
                if [[ ! -f "$S/guest.silent" ]]; then guest_line >>"$port"; guest_sent; fi
            fi
        fi
        # The holder dies (killed, out of memory) once the VM runs.
        if [[ -f "$S/kill_holder_at_start" ]]; then kill -KILL "$(head -n 1 "$S/holder.pids")"; fi
        # ...and from now on something keeps the drive busy.
        if [[ -f "$S/busy_after_start" ]]; then touch "$S/hold_busy"; fi
        # Something on the host mounts a partition while the session runs.
        if [[ -f "$S/lsblk.mounts.sdj.after_start" ]]; then cp "$S/lsblk.mounts.sdj.after_start" "$S/lsblk.mounts.sdj"; fi
        echo "Domain 'recovery-os-updater' resumed"
        ;;
    destroy)
        # Only a domain started paused and never resumed may be destroyed:
        # it has run nothing. Anything else is a running recovery OS.
        if [[ -f "$S/resumed" || -f "$S/vcpu_ran" || ! -f "$S/started" || "$(head -n 1 "$S/states")" != paused ]]; then
            echo "destroy of a domain that was resumed, is not paused, or never started ($(head -n 1 "$S/states")): $*" >>"$STUB/forbidden"
            echo "destroy after a resume (or of one not paused, or never started): $S" >>"${DESTROY_LOG:-/dev/null}"
            exit 1
        fi
        echo "destroy, never resumed: $S" >>"${DESTROY_LOG:-/dev/null}"
        echo "virsh destroy" >>"$S/events"
        if [[ -f "$S/destroy_fail" ]]; then echo "error: Failed to destroy domain" >&2; exit 1; fi
        printf 'shut off\n' >"$S/states"
        echo "Domain 'recovery-os-updater' destroyed"
        ;;
    shutdown)
        echo "virsh shutdown" >>"$S/events"
        # A guest that drops the first N requests (firmware, boot menu, initrd).
        if [[ -f "$S/shutdown_ignore" ]] && (($(cat "$S/shutdown_ignore") > 0)); then
            echo $(($(cat "$S/shutdown_ignore") - 1)) >"$S/shutdown_ignore"
        elif [[ -f "$S/states.after_shutdown" ]]; then
            cp "$S/states.after_shutdown" "$S/states"
        fi
        echo "Domain 'recovery-os-updater' is being shutdown"
        ;;
    define)
        if [[ -f "$S/define_fail_guarded" ]] && grep -q 'org.dasbackup.guard' "${!#}"; then
            echo "error: Failed to define domain from ${!#}" >&2
            exit 1
        fi
        if [[ -f "$S/define_fail_plain" ]] && ! grep -q 'org.dasbackup.guard' "${!#}"; then
            echo "error: Failed to define domain from ${!#}" >&2
            exit 1
        fi
        echo "virsh define" >>"$S/events"
        # Who holds the maintenance lock while the definition changes.
        { head -n 1 "$DAS_RECOVERY_VM_TEST_ROOT/run/das-maintenance.lock" 2>/dev/null || echo "(no lock file)"; } >>"$S/lock.at_define"
        touch "$S/defined"
        if [[ -f "$S/define_keeps_guard" ]] && ! grep -q 'org.dasbackup.guard' "${!#}"; then
            echo "Domain 'recovery-os-updater' defined from ${!#}"
            exit 0
        fi
        cp "${!#}" "$S/defined.xml"
        # A libvirt that keeps no SMBIOS strings.
        if [[ -f "$S/define_drops_guard" ]]; then sed -i '/<entry>/d' "$S/defined.xml"; fi
        rm -f "$S/stored-os.xml"
        echo "Domain 'recovery-os-updater' defined from ${!#}"
        ;;
    net-dumpxml)
        [[ "$*" == default ]] || { echo "UNEXPECTED net-dumpxml $*" >>"$STUB/forbidden"; exit 99; }
        if [[ -f "$STUB/net_fail" ]]; then echo "error: failed to get network 'default'" >&2; exit 1; fi
        if [[ -f "$STUB/net.xml" ]]; then cat "$STUB/net.xml"; exit 0; fi
        printf "<network>\n  <name>default</name>\n  <forward mode='nat'/>\n  <ip address='192.168.122.1' netmask='255.255.255.0'>\n    <dhcp>\n      <range start='192.168.122.2' end='192.168.122.254'/>\n    </dhcp>\n  </ip>\n</network>\n"
        ;;
    undefine)
        [[ "$*" == "--nvram recovery-os-updater" ]] || { echo "UNEXPECTED undefine $*" >>"$STUB/forbidden"; exit 99; }
        [[ -f "$S/defined" ]] || nodomain
        echo "virsh undefine $DOMNAME" >>"$STUB/events"
        if [[ -f "$S/undefine_fail" ]]; then echo "error: Failed to undefine domain" >&2; exit 1; fi
        rm -f "$S/defined"
        echo "Domain '$DOMNAME' has been undefined"
        ;;
    snapshot-list)
        [[ "${1:-}" == --name ]] || { echo "UNEXPECTED snapshot-list $*" >>"$STUB/forbidden"; exit 99; }
        [[ -f "$S/defined" ]] || nodomain
        cat "$S/snapshots" 2>/dev/null || :
        echo
        ;;
    qemu-agent-command)
        # The recovery OS's guest agent, modelled: guest-ping, guest-shutdown,
        # guest-exec (the driver's own scripts recognised by their shape, never
        # run here) and guest-exec-status. agent_down: it never answers.
        [[ "${2:-}" == --timeout ]] || { echo "UNEXPECTED qemu-agent-command $*" >>"$STUB/forbidden"; exit 99; }
        json=${!#}
        printf '%s\n' "$json" >>"$S/agent.calls"
        if [[ -f "$S/agent_down" || "$(head -n 1 "$S/states")" != running ]]; then
            echo "error: Guest agent is not responding: QEMU guest agent is not connected" >&2
            exit 1
        fi
        what=$(jq -r '.execute' <<<"$json")
        mkdir -p "$S/ge"
        case "$what" in
            guest-ping) echo '{"return":{}}' ;;
            guest-shutdown)
                echo "guest shutdown" >>"$STUB/events"
                if [[ ! -f "$S/guest_ignores_shutdown" ]]; then printf 'shut off\n' >"$S/states"; fi
                echo '{"return":{}}'
                ;;
            guest-exec)
                path=$(jq -r '.arguments.path' <<<"$json")
                script=$(jq -r '.arguments.arg[1] // .arguments.arg[0] // empty' <<<"$json")
                n=$(($(cat "$S/ge/next" 2>/dev/null || echo 1000)))
                echo $((n + 1)) >"$S/ge/next"
                rc=0 out=""
                if [[ "$path" == /usr/bin/systemctl && "$script" == reboot ]]; then
                    # A reboot: libvirt's reset event first, then the new boot.
                    echo "guest reboot" >>"$STUB/events"
                    before=$(resets_seen)
                    echo reset >>"$S/reboots.pending"
                    for ((i = 0; i < 300; i++)); do (($(resets_seen) > before)) && break; sleep 0.01; done
                    if [[ ! -f "$S/reboot_unguarded" ]]; then
                        echo "${REBOOT_BOOT:-bbbbbbbb-1111-4222-8333-444444444444}" >"$S/guest.boot"
                        echo 0 >"$S/guest.seq"
                        echo engaged >"$S/guest.mode"
                        : >"$S/guest.last"
                    else
                        touch "$S/guest.silent"
                    fi
                elif [[ "$script" == *"systemd-run --quiet --collect --no-block --unit=das-vm-update-"* ]]; then
                    unit=${script##*--unit=das-vm-update-}
                    unit=${unit%% *}
                    k=${unit%%-*}
                    stage=${unit#*-}
                    echo "$stage" >>"$STUB/stages.log"
                    echo "$stage" >"$S/ge/stage.$k"
                    b64=$(sed -n "s#.*printf '%s' '\([^']*\)' | base64 -d >/run/das-vm-update/$k\.sh.*#\1#p" <<<"$script")
                    printf '%s' "$b64" | base64 -d >"$STUB/stage.$stage.sh" 2>/dev/null || :
                    echo 0 >"$S/ge/probes.$k"
                    if [[ -f "$STUB/stage.$stage.launch_fails" ]]; then rc=1 out="Failed to start transient service unit"; fi
                    case "$stage" in
                        guard-lift) echo lifted >"$S/guest.mode" ;;
                        guard-engage) echo engaged >"$S/guest.mode" ;;
                    esac
                elif [[ "$script" == *"/run/das-vm-update/"*".rc ]; then r=\$(cat"* ]]; then
                    k=${script#*/run/das-vm-update/}
                    k=${k%%.rc*}
                    stage=$(cat "$S/ge/stage.$k")
                    off=${script#*tail -c +}
                    off=${off%% *}
                    p=$(($(cat "$S/ge/probes.$k") + 1))
                    echo "$p" >"$S/ge/probes.$k"
                    # Each step ends at its second look, or when told.
                    want=$(cat "$STUB/stage.$stage.polls" 2>/dev/null || echo 2)
                    full=$(cat "$STUB/stage.$stage.out" 2>/dev/null || printf 'did %s\n' "$stage")
                    r=""
                    if ((p >= want)); then r=$(cat "$STUB/stage.$stage.rc" 2>/dev/null || echo 0); fi
                    if [[ -f "$STUB/stage.$stage.hangs" ]]; then r=""; fi
                    chunk=$(printf '%s' "$full" | tail -c +"$off" | head -c 65536 | base64 -w0)
                    out=$(printf 'rc=%s\n%s\n' "$r" "$chunk")
                elif [[ "$script" == "cat /proc/sys/kernel/random/boot_id" ]]; then
                    out=$(cat "$S/guest.boot")
                elif [[ "$script" == "uname -r" ]]; then
                    out=$(cat "$STUB/guest.kernel" 2>/dev/null || echo 7.2.9-1-cachyos)
                else
                    echo "UNEXPECTED guest-exec $path $script" >>"$STUB/forbidden"
                    rc=127
                fi
                jq -cn --argjson rc "$rc" --arg out "$(printf '%s' "$out" | base64 -w0)" \
                    '{return: {exited: true, exitcode: $rc, "out-data": $out}}' >"$S/ge/$n.json"
                echo "{\"return\":{\"pid\":$n}}"
                ;;
            guest-exec-status)
                pid=$(jq -r '.arguments.pid' <<<"$json")
                cat "$S/ge/$pid.json" 2>/dev/null || { echo "error: internal error: Invalid parameter 'pid'" >&2; exit 1; }
                ;;
            *) echo "UNEXPECTED agent $what" >>"$STUB/forbidden"; exit 99 ;;
        esac
        ;;
    screenshot)
        printf 'P6\n1 1\n255\n...' >"$2"
        echo "Screenshot saved to $2, with type of image/x-portable-pixmap"
        ;;
    *)
        echo "stub virsh: unexpected '$cmd $*'" >&2
        echo "UNEXPECTED $cmd $*" >>"$STUB/forbidden"
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
# The instant the driver says it let the lock go: is it free? (Nothing it
# started -- the reset watch included -- may still hold its descriptor.)
if [[ "$*" == *"released the DAS maintenance lock"* ]]; then
    if "$REAL_FLOCK" -n "$DAS_RECOVERY_VM_TEST_ROOT/run/das-maintenance.lock" true; then echo free; else echo held; fi >>"$STUB/lock.at_release"
fi
STUB

    cat >"$T/bin/sync" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/sync.calls"
STUB

    cat >"$T/bin/stat" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/stat.calls"
# A device the test describes; anything else is the real stat (the report).
if [[ -f "$STUB/stat.$(basename "${!#}")" ]]; then cat "$STUB/stat.$(basename "${!#}")"; exit 0; fi
exec "$REAL_STAT" "$@"
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

    # ip: the egress rule (rules at priority 5100 kept one "subnet/len" a
    # line in stub/ip.rules). Variants: show, add or del fails; an add that
    # says it worked and put nothing in place.
    cat >"$T/bin/ip" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/ip.calls"
rules="$STUB/ip.rules"
case "$*" in
    "-j rule show priority 5100")
        if [[ -f "$STUB/ip_show_fail_next" ]]; then rm -f "$STUB/ip_show_fail_next"; echo "RTNETLINK answers: Operation not permitted" >&2; exit 2; fi
        if [[ -f "$STUB/ip_show_fail" ]]; then echo "RTNETLINK answers: Operation not permitted" >&2; exit 2; fi
        printf '['
        sep=""
        while read -r r; do
            [[ -n "$r" ]] || continue
            printf '%s{"priority":5100,"src":"%s","srclen":%s,"table":"main"}' "$sep" "${r%/*}" "${r#*/}"
            sep=,
        done < <(cat "$rules" 2>/dev/null)
        printf ']\n'
        ;;
    "rule show priority 5100")
        if [[ -f "$STUB/ip_show_fail" ]]; then echo "RTNETLINK answers: Operation not permitted" >&2; exit 2; fi
        while read -r r; do [[ -z "$r" ]] || printf '5100:\tfrom %s lookup main\n' "$r"; done < <(cat "$rules" 2>/dev/null)
        ;;
    "rule add from "*" lookup main priority 5100")
        if [[ -f "$STUB/ip_add_fail" ]]; then echo "RTNETLINK answers: Operation not permitted" >&2; exit 2; fi
        read -r _ _ _ subnet _ <<<"$*"
        [[ -f "$STUB/ip_add_noop" ]] || echo "$subnet" >>"$rules"
        if [[ -f "$STUB/ip_show_fail_after_add" ]]; then touch "$STUB/ip_show_fail_next"; fi
        ;;
    "rule del from "*" lookup main priority 5100")
        if [[ -f "$STUB/ip_del_fail" ]]; then echo "RTNETLINK answers: Operation not permitted" >&2; exit 2; fi
        read -r _ _ _ subnet _ <<<"$*"
        if ! grep -qxF -- "$subnet" "$rules" 2>/dev/null; then echo "RTNETLINK answers: No such file or directory" >&2; exit 2; fi
        awk -v s="$subnet" '$0 == s && !done { done = 1; next } { print }' "$rules" >"$rules.new" && mv "$rules.new" "$rules"
        ;;
    *) echo "stub ip: unexpected $*" >&2; echo "UNEXPECTED ip $*" >>"$STUB/forbidden"; exit 99 ;;
esac
STUB

    # socat for the console bridge: a real listening socket at the path its
    # UNIX-LISTEN names, accepting once; "socat" in its command line, as the
    # real one's. Only the options it is given shape the socket, as socat's
    # do: mode= is applied, else the umask decides; user= is applied (this
    # suite runs unprivileged, so only to the runner itself), else the socket
    # would be root's -- which an unprivileged stub cannot make, so it says so
    # and makes none. unlink-early removes what is at the path first; an
    # option it does not know is refused, as socat refuses one.
    cat >"$T/bin/socat" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$STUB/socat.calls"
if [[ -f "$STUB/socat_fail" ]]; then echo "socat: E bind: Permission denied" >&2; exit 1; fi
exec -a socat python3 -c '
import os, pwd, socket, sys
addr = sys.argv[1]
if not addr.startswith("UNIX-LISTEN:"):
    sys.exit("socat stub: E first address is not UNIX-LISTEN")
path, *opts = addr[len("UNIX-LISTEN:"):].split(",")
o = {}
for opt in opts:
    k, _, v = opt.partition("=")
    if k not in ("mode", "user", "group", "unlink-early"):
        sys.exit("socat stub: E unknown option " + k)
    o[k] = v
if "user" not in o:
    sys.exit("socat stub: E no user= -- the socket would be root\x27s")
uid = int(o["user"]) if o["user"].isdigit() else pwd.getpwnam(o["user"]).pw_uid
gid = int(o["group"]) if o.get("group", "").isdigit() else -1
if "unlink-early" in o:
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
s = socket.socket(socket.AF_UNIX)
s.bind(path)
if "mode" in o:
    os.chmod(path, int(o["mode"], 8))
os.chown(path, uid, gid)
s.listen(1)
c, _ = s.accept()
c.close()
' "$@"
STUB

    chmod +x "$T/bin/"*
}

driver_env() {
    # GUARD_SECS=default: the knob left unset, so the driver's own default runs.
    local guard=(DAS_RECOVERY_VM_GUARD_SECS="${GUARD_SECS:-2}")
    if [[ "${GUARD_SECS:-}" == default ]]; then guard=(); fi
    env -u DAS_RECOVERY_OS_STATE -u DAS_RECOVERY_VM_GUARD_SECS ${OS_STATE:+DAS_RECOVERY_OS_STATE="$OS_STATE"} \
        PATH="${DRIVER_PATH:-$T/bin:$PATH}" STUB="$S" REAL_FLOCK="$REAL_FLOCK" REAL_STAT="$REAL_STAT" \
        DAS_RECOVERY_VM_TEST_ROOT="$T" BTRDASD_BIN="$T/bin/btrdasd" \
        DAS_RECOVERY_VM_POLL_SECS="${POLL:-0.05}" DAS_RECOVERY_VM_MINUTE_SECS=1 DAS_RECOVERY_VM_GRACE_SECS="${GRACE:-1}" \
        DAS_RECOVERY_VM_RESEND_SECS="${RESEND:-30}" DAS_RECOVERY_VM_CLOCK_GAP_SECS="${CLOCK_GAP:-30}" "${guard[@]}" \
        ${LOOP:+DAS_RECOVERY_VM_TEST_LOOP="$LOOP"} "$@"
}

# Run the driver to completion: OUT is stdout and stderr together, RC its
# status. Bounded, so a driver that hangs fails its test instead of the suite
# never ending (124 or 137 then, never an expected status).
run_driver() {
    RC=0
    OUT="$(driver_env timeout -k 5 "${DRIVER_TIMEOUT:-60}" bash "$DRIVER" "$@" 2>&1)" || RC=$?
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
    if [[ "$line" != *" pid "* || ! "$pid" =~ ^[[:digit:]]+$ ]]; then
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
    env -u DAS_RECOVERY_OS_STATE PATH="$T/bin:$PATH" STUB="$S" REAL_FLOCK="$REAL_FLOCK" REAL_STAT="$REAL_STAT" \
        DAS_RECOVERY_VM_TEST_ROOT="$T" BTRDASD_BIN="$T/bin/btrdasd" \
        DAS_RECOVERY_VM_POLL_SECS="${POLL:-0.05}" DAS_RECOVERY_VM_MINUTE_SECS=1 DAS_RECOVERY_VM_GRACE_SECS="${GRACE:-1}" \
        DAS_RECOVERY_VM_RESEND_SECS="${RESEND:-30}" DAS_RECOVERY_VM_GUARD_SECS="${GUARD_SECS:-2}" \
        DAS_RECOVERY_VM_CLOCK_GAP_SECS="${CLOCK_GAP:-30}" \
        bash "$DRIVER" "$@" >"$T/driver.out" 2>&1 &
    dpid=$!
    set +m
    for ((i = 0; i < 200; i++)); do
        if [[ -n "${WAIT_FILE:-}" ]]; then
            [[ ! -e "$WAIT_FILE" ]] || break
        elif grep -q 'waiting for the recovery OS to power off' "$T/driver.out"; then
            break
        fi
        sleep 0.05
    done
    sleep 0.2
    # What happens in the guest in the instant before the signal.
    if [[ -n "${BEFORE_SIG:-}" ]]; then eval "$BEFORE_SIG"; fi
    kill -"${SIG:-INT}" -- "-$dpid" 2>/dev/null || echo "(the driver had exited before SIG${SIG:-INT})" >>"$T/driver.out"
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

# The driver in the background, stopped -- SIGSTOP to its whole process
# group, the guest's stand-ins with it, as a host suspend stops everything --
# for STOP_SECS once it waits for the guest, then let go on to its end.
run_stopped() {
    local i dpid
    RC=0
    set -m
    env -u DAS_RECOVERY_OS_STATE PATH="$T/bin:$PATH" STUB="$S" REAL_FLOCK="$REAL_FLOCK" REAL_STAT="$REAL_STAT" \
        DAS_RECOVERY_VM_TEST_ROOT="$T" BTRDASD_BIN="$T/bin/btrdasd" \
        DAS_RECOVERY_VM_POLL_SECS="${POLL:-0.05}" DAS_RECOVERY_VM_MINUTE_SECS=1 DAS_RECOVERY_VM_GRACE_SECS="${GRACE:-1}" \
        DAS_RECOVERY_VM_RESEND_SECS="${RESEND:-30}" DAS_RECOVERY_VM_GUARD_SECS="${GUARD_SECS:-2}" \
        DAS_RECOVERY_VM_CLOCK_GAP_SECS="${CLOCK_GAP:-30}" \
        bash "$DRIVER" "$@" >"$T/driver.out" 2>&1 &
    dpid=$!
    set +m
    for ((i = 0; i < 200; i++)); do
        if grep -q 'waiting for the recovery OS to power off' "$T/driver.out"; then break; fi
        sleep 0.05
    done
    sleep 0.2
    kill -STOP -- "-$dpid" 2>/dev/null || echo "(the driver had exited before SIGSTOP)" >>"$T/driver.out"
    sleep "${STOP_SECS:-3}"
    kill -CONT -- "-$dpid" 2>/dev/null || :
    for ((i = 0; i < 1200; i++)); do
        kill -0 "$dpid" 2>/dev/null || break
        sleep 0.05
    done
    if kill -0 "$dpid" 2>/dev/null; then
        kill -KILL -- "-$dpid" 2>/dev/null || :
        echo "(the driver did not end within 60s of SIGCONT)" >>"$T/driver.out"
    fi
    wait "$dpid" || RC=$?
    OUT="$(cat "$T/driver.out")"
}

# Whether every reset watch a driver started was ended by that driver on its
# way out (the stub's virsh event says whether the driver still ran then).
# Not after a Ctrl-C: that reaches the stub's virsh event itself, as it would
# reach the real one, so the answer says nothing about the driver.
watch_ended() {
    local started ended
    started=$(grep -c '^started$' "$S/event.log" 2>/dev/null) || started=0
    ended=$(grep -c '^ended while the driver ran$' "$S/event.log" 2>/dev/null) || ended=0
    echo "$started started, $ended ended by the driver"
}

# The reset watch of driver $2: the child of it that reads the output of
# virsh event $1 -- holds the read end of that virsh's stdout -- found by
# provenance, never by name. Fails when there is none.
reset_watch_of() {
    local epid=$1 dpid=$2 pipe p st ppid f
    pipe=$(readlink "/proc/$epid/fd/1") || return 1
    [[ "$pipe" == pipe:* ]] || return 1
    for p in /proc/[0-9]*; do
        p=${p#/proc/}
        [[ "$p" != "$epid" ]] || continue
        { read -r st <"/proc/$p/stat"; } 2>/dev/null || continue
        read -r _ ppid _ <<<"${st##*) }"
        [[ "$ppid" == "$dpid" ]] || continue
        for f in "/proc/$p/fd/"*; do
            if [[ "$(readlink "$f" 2>/dev/null)" == "$pipe" ]]; then
                echo "$p"
                return 0
            fi
        done
    done
    return 1
}

# The driver in the background; once it waits for the guest, its reset watch
# is stopped (SIGSTOP) and never let go -- a watch that cannot end on the
# SIGTERM it is sent -- and the driver runs on to its end, 60 s at most.
# WATCH_PID is the watch it stopped ("" when none was found).
run_watch_hung() {
    local i dpid epid
    RC=0 WATCH_PID=""
    set -m
    driver_env bash "$DRIVER" "$@" >"$T/driver.out" 2>&1 &
    dpid=$!
    set +m
    for ((i = 0; i < 200; i++)); do
        grep -q 'waiting for the recovery OS to power off' "$T/driver.out" && break
        sleep 0.05
    done
    epid=$(head -n 1 "$S/event.pids") || epid=""
    if [[ -n "$epid" ]] && WATCH_PID=$(reset_watch_of "$epid" "$dpid"); then
        kill -STOP "$WATCH_PID"
    else
        WATCH_PID=""
        echo "(the reset watch was not found: no child of the driver $dpid reads virsh event $epid)" >>"$T/driver.out"
    fi
    for ((i = 0; i < 1200; i++)); do
        kill -0 "$dpid" 2>/dev/null || break
        sleep 0.05
    done
    if kill -0 "$dpid" 2>/dev/null; then
        kill -KILL -- "-$dpid" 2>/dev/null || :
        echo "(the driver did not end within 60s)" >>"$T/driver.out"
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
run_driver session A B C
check "three targets: exit 2" "$RC" "2"
run_driver session A --mode parallel
check "--mode with one drive: exit 2" "$RC" "2"
run_driver session A B --mode sideways
check "--mode sideways: exit 2" "$RC" "2"
run_driver screenshot A
check "screenshot without a path: exit 2" "$RC" "2"
run_driver console-socket A
check "console-socket without a user: exit 2" "$RC" "2"
run_driver clean-runs
check "clean-runs without a drive: exit 2" "$RC" "2"
run_driver session-end ''
check "session-end with an empty label: exit 2" "$RC" "2"
run_driver screenshot A ''
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
check "domain name: the template's, every drive's domain named after it" "$(sed -n 's|.*<name>\(.*\)</name>.*|\1|p' "$xml")" \
    "$(sed -n 's/^readonly DOMAIN_BASE="\(.*\)"$/\1/p' "$REPO/scripts/recovery-os-vm.sh")"
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
# Every session defines the domain anew (with the guard, then without): the
# guest's network card must keep one address.
has "the NIC keeps one MAC address across sessions" "$(sed -n '/<interface /,/<\/interface>/p' "$xml")" "<mac address='52:54:00:"
lacks "the template carries no credentials" "$(cat "$xml")" "credential"
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
has "domain running: says so" "$OUT" "recovery-os-updater-system-recovery-A-2tb is running -- it must be shut off"

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
has "session: attached to the persistent definition" "$(file "$S/virsh.calls")" "attach-device recovery-os-updater-system-recovery-A-2tb"
check "session: events in order" "$(events)" \
    "holder in its own session|holder started (lock held)|virsh define|virsh attach|virsh start|virsh resume|virsh detach|virsh define|holder released|btrfs scan (lock held)|"
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
check "session: free the instant the driver let it go (nothing it started holds it)" "$(file "$S/lock.at_release")" "free"
check "session: no record left" "$(ls -A "$STATE")" ""
has "session: console shown, through libvirt" "$OUT" "virt-viewer --connect qemu:///system --attach recovery-os-updater-system-recovery-A-2tb"
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
has "start failure: detached and released" "$(events)" "virsh start|virsh detach|virsh define|holder released|"
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
has "SIGINT: the guard engaged, and when it last reported" "$OUT" "the session guard is engaged (das-vm-guard engaged 4 masks btrbk.service,btrbk.timer,cronie.service,crond.service; its last report 0h 00m 0"
has "SIGINT: ...and that status judges it from here" "$OUT" "From here nothing watches it but $T/usr/lib/das-backup/recovery-os-vm.sh status, which judges its report again"
has "SIGINT: ...and that status counts silence, blind to resets after this script" "$OUT" "also counts 0h 00m 02s of silence as NOT confirmed, since it cannot see a reset made after this script ended"
matches "SIGINT: the record stays while the session holds the lock" "$(head -n 1 "$LOCK")" "$RECORD_A"
has "SIGINT: never stop the holder's scope" "$OUT" "Never 'systemctl stop' das-recovery-os-holder-system-recovery-A-2tb.scope"
check "SIGINT: the record names the holder, which outlives the driver" "$(head -n 1 "$LOCK")" \
    "recovery-os VM session system-recovery-A-2tb pid $pid"
check "SIGINT: a reader sees the holder alive, not a finished session" "$(frb_reader)" \
    "recovery-os VM session system-recovery-A-2tb pid $pid"

run_driver status
check "status: exit 0" "$RC" "0"
has "status: domain state" "$OUT" "recovery-os-updater-system-recovery-A-2tb (system-recovery-A-2tb): running"
has "status: attached disk" "$OUT" "  Attached disk   $DISK_A"
has "status: holder alive" "$OUT" "system-recovery-A-2tb: pid $pid, alive"
has "status: lock holder" "$OUT" "held by: recovery-os VM session system-recovery-A-2tb"
has "status: the holder's scope" "$OUT" "scope das-recovery-os-holder-system-recovery-A-2tb.scope: active"
has "status: the session's guard in the definition" "$OUT" "  Session guard   in the definition"

run_driver session-end A
check "session-end while running: refused" "$RC" "1"
has "session-end while running: says why" "$OUT" "never stops a running recovery OS"
check "session-end while running: holder kept" "$(alive "$pid")" "alive"
check "session-end while running: lock kept" "$(lock_state)" "held"

printf 'shut off\n' >"$S/states"
run_driver session-end system-recovery-A-2tb
check "session-end once shut off: exit 0" "$RC" "0"
has "session-end: detached, holder stopped, rescanned" "$(events)" "virsh detach|virsh define|holder released|btrfs scan"
lacks "session-end: the guard out of the definition" "$(file "$S/defined.xml")" "io.systemd.credential"
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
has "status when idle: no guard" "$OUT" "  Session guard   none"
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
has "timeout, guest shuts down: given back" "$(events)" "virsh shutdown|virsh detach|virsh define|holder released|btrfs scan (lock held)|"
check "timeout, guest shuts down: lock free" "$(lock_state)" "free"

echo "--- the disk cannot be given back"
fixture
touch "$S/detach_fail"
run_driver session A
check "detach fails: exit 4" "$RC" "4"
pid="$(holder_pid A)"
check "detach fails: claim kept" "$(alive "$pid")" "alive"
check "detach fails: lock kept" "$(lock_state)" "held"
has "detach fails: says so" "$OUT" "still in recovery-os-updater-system-recovery-A-2tb's definition"
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
    "virsh start|virsh resume|holder in its own session|holder started (lock held)|"
has "holder lost: then given back as usual" "$(events)" "virsh detach|virsh define|holder released|btrfs scan (lock held)|"
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
has "a label a domain name cannot carry: says so" "$OUT" "characters this script will not put in a libvirt domain's name"
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

# Attended, "will" goes on with a banner (decision 3 of bd 8249): the guard
# stops btrbk, the operator disables it in the session.
fixture
write_state 3 "$(record_json system-recovery-A-2tb will)" "$(record_json system-recovery-B-2tb no)"
run_driver session A --dry-run
check "record will, attended: the session goes on (decision 3)" "$RC" "0"
has "record will, attended: the banner" "$OUT" "WARNING: this OS runs btrbk at boot; the guard is stopping it now; disable it in this session"
has "record will: the reason shown" "$OUT" "  - btrbk.timer starts btrbk.service, which runs btrbk at its next scheduled time after boot"
has "record will: the verdict shown" "$OUT" "btrbk at boot  will"
lacks "record will, attended: not refused" "$OUT" "REFUSED"
lacks "record will, attended: nothing overridden" "$OUT" "OVERRIDDEN"
fixture
write_state 3 "$(record_json system-recovery-A-2tb will)"
run_driver session A
check "record will, attended, a real session: ends clean" "$RC" "0"
check "record will, attended, a real session: the banner before the boot and again once the guard confirms" "$(grep -c 'WARNING: this OS runs btrbk at boot; the guard is stopping it now' <<<"$OUT")" "2"
fixture
run_driver session A
lacks "record no, a real session: no banner" "$OUT" "this OS runs btrbk at boot"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
run_driver session A --dry-run
has "record may: the reason shown" "$OUT" "  - not read"
check "record may: the session goes on, the guard enforces" "$RC" "0"
has "record may: says the guard is what enforces" "$OUT" "btrbk may run when this OS boots: the session guard is what keeps it from running"

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
has "schema 2: says so" "$OUT" "is schema 2 -- this script reads schemas 3 and 4 only"
fixture
write_state 5 "$(record_json system-recovery-A-2tb no)"
run_driver session A --dry-run
check "schema 5, newer: refused too" "$RC" "1"
has "schema 5: says so" "$OUT" "is schema 5 -- this script reads schemas 3 and 4 only"
fixture
write_state 4 "$(record_json system-recovery-A-2tb no)"
run_driver session A --dry-run
check "schema 4 (the guest agent added): read" "$RC" "0"

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
check "override, will: nothing to override attended (exit 0)" "$RC" "0"
lacks "override, will: nothing said overridden" "$OUT" "OVERRIDDEN"

fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
run_driver session A --dry-run --accept-boot-record-risk
check "override, may: nothing to override (exit 0)" "$RC" "0"

fixture
write_state 3 "$(record_json system-recovery-A-2tb no $((9 * 86400)))"
run_driver session A --dry-run --accept-boot-record-risk
check "override, 9 days old: goes on, flagged (exit 5)" "$RC" "5"
has "override, 9 days old: said loudly" "$OUT" "OVERRIDDEN (--accept-boot-record-risk): the record is 9d 0h old"
has "override, 9 days old: the dry run says why it exits 5" "$OUT" "dry run: the boot-record check was overridden"

fixture
write_state 3 "$(record_json system-recovery-A-2tb no 7200)"
printf 'system-recovery-A-2tb %s\n' "$(($(date +%s) - 3600))" >"$SESSIONS"
run_driver session A --dry-run --accept-boot-record-risk
check "override, before the last session: goes on, flagged (exit 5)" "$RC" "5"
has "override, before the last session: said loudly" "$OUT" "OVERRIDDEN (--accept-boot-record-risk): the record was made before this drive's last VM session ended"

fixture
write_state 3 "$(record_json system-recovery-A-2tb no $((9 * 86400)))"
run_driver session A --accept-boot-record-risk
check "override, a real session: ends, flagged" "$RC" "5"
has "override, a real session: in the summary's warnings" "$OUT" "Warnings      the boot-record check was overridden (--accept-boot-record-risk): the record is 9d 0h old"

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
has "override never covers another schema: that refusal, not another" "$OUT" "is schema 2 -- this script reads"
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
has "a string schema: said as such" "$OUT" "schema_version is not a number"
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

echo "--- the boot record: the filesystem it was read from (bd DAS-Backup-Manager-df0)"
# A record describes the filesystem it was read from, not a label: a label
# pointed at another drive, or a filesystem made again, must never inherit the
# old drive's verdict. Never overridable.
fixture
run_driver session A --dry-run
check "df0, the record names config's filesystem: goes on" "$RC" "0"
has "df0, the record names config's filesystem: shown" "$OUT" "  filesystem     $UUID_A (the mount_uuid of system-recovery-A-2tb)"
for f in ABSENT null '"unknown"' '""' 7; do
    for o in "" --accept-boot-record-risk; do
        fixture
        write_state 3 "$(record_json system-recovery-A-2tb no 3600 "$f")"
        run_driver session A --dry-run $o
        check "df0, a record naming no filesystem ($f)${o:+, $o}: refused" "$RC" "1"
        has "df0, no filesystem ($f)${o:+, $o}: says so" "$OUT" "does not name the filesystem it was read from"
        check "df0, no filesystem ($f)${o:+, $o}: nothing locked" "$(file "$S/flock.calls")" ""
        check "df0, no filesystem ($f)${o:+, $o}: nothing held" "$(file "$S/holder.calls")" ""
    done
done
for o in "" --accept-boot-record-risk; do
    fixture
    write_state 3 "$(record_json system-recovery-A-2tb no 3600 '"ffffffff-0000-4000-8000-000000000000"')"
    run_driver session A --dry-run $o
    check "df0, a record of another filesystem${o:+, $o}: refused" "$RC" "1"
    has "df0, another filesystem${o:+, $o}: says which, and which config names" "$OUT" "was read from filesystem ffffffff-0000-4000-8000-000000000000, not $UUID_A, the mount_uuid of 'system-recovery-A-2tb'"
    check "df0, another filesystem${o:+, $o}: nothing locked" "$(file "$S/flock.calls")" ""
    check "df0, another filesystem${o:+, $o}: nothing held" "$(file "$S/holder.calls")" ""
done
# The filesystem was made again: config and partition 2 both name the new
# one, and the identity check passes -- only the record is of the old one.
fixture
dump_env ZK208Q77 ZFL41DNY system-recovery-B-2tb "ffffffff-0000-4000-8000-000000000000"
printf '%s\n' "ffffffff-0000-4000-8000-000000000000" >"$S/lsblk.uuid.sdj2"
run_driver session A --dry-run --accept-boot-record-risk
check "df0, a filesystem made again since the record: refused" "$RC" "1"
has "df0, made again: says so" "$OUT" "was read from filesystem $UUID_A, not ffffffff-0000-4000-8000-000000000000"
check "df0, made again: nothing held" "$(file "$S/holder.calls")" ""
write_state 3 "$(record_json system-recovery-A-2tb no)" "$(record_json system-recovery-B-2tb no)"
run_driver session A --dry-run
check "df0, made again, then recorded again: goes on" "$RC" "0"
# The label pointed at another drive: config now gives A's label B's drive
# (serial and filesystem); A's record is still of A's old drive.
fixture
dump_env ZFL41DNY ZFL41DNY system-recovery-B-2tb "$UUID_B"
run_driver session A --dry-run --accept-boot-record-risk
check "df0, the label pointed at another drive: refused" "$RC" "1"
has "df0, another drive: says so" "$OUT" "was read from filesystem $UUID_A, not $UUID_B"
check "df0, another drive: never held" "$(file "$S/holder.calls")" ""
# Partition 2 is checked against config, and config against the record: a
# record and config that agree, and a disk that does not, is refused too.
fixture
printf '%s\n' "ffffffff-0000-4000-8000-000000000000" >"$S/lsblk.uuid.sdj2"
run_driver session A --dry-run --accept-boot-record-risk
check "df0, the record and config agree, partition 2 does not: refused" "$RC" "1"
has "df0, partition 2 differs: says which" "$OUT" "carries filesystem ffffffff-0000-4000-8000-000000000000, not $UUID_A"
check "df0, partition 2 differs: never held" "$(file "$S/holder.calls")" ""
# No mount_uuid in config: neither the drive nor its record can be tied to a
# filesystem.
fixture
dump_env ZK208Q77 ZFL41DNY system-recovery-B-2tb ""
run_driver session A --dry-run --accept-boot-record-risk
check "df0, no mount_uuid in config: refused" "$RC" "1"
has "df0, no mount_uuid in config: says so" "$OUT" "has no mount_uuid in"
check "df0, no mount_uuid in config: nothing locked" "$(file "$S/flock.calls")" ""

echo "--- the contract: the boot record as the real btrdasd writes it (bd 1yg, df0)"
# Every test above reads a record this suite wrote to the contract. These read
# one the real btrdasd wrote. `recovery-os status --state-file` reads only a
# mounted target, and nothing here may mount, so the record is made in two
# steps, each by the binary: `recovery-os inspect --json` reads a fixture root
# as the drive's OS -- its `os` object is the very value the record stores per
# drive -- and that reading is seeded as the drive's last record, with a key
# btrdasd does not know; then `recovery-os status --state-file`, the drive not
# mounted, rewrites the whole file through btrdasd's own record types, keeping
# the drive's record as it was. The file the driver reads is the binary's: the
# unknown key gone proves the rewrite, and a mount_uuid the types did not keep
# would come back null, which the driver refuses. What only a mounted drive
# shows -- status writing the mount_uuid it read -- is the Rust tests' (and the
# first real run's): recovery_os.rs, status_records_the_filesystem_each_...
if [[ -z "${REAL_BTRDASD:-}" ]]; then
    echo "NOT RUN: the contract with the real btrdasd -- set REAL_BTRDASD to a built btrdasd (ctest does)"
else
    # A config the real btrdasd accepts, with the mirror target and the
    # mount_uuid this suite's dump-env gives system-recovery-A-2tb; its mount
    # point does not exist, so status finds the drive not mounted.
    contract_config() {
        cat >"$CW/config.toml" <<EOF
[general]
version = "0.7.22"
install_prefix = "/usr"
db_path = "$CW/index.db"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[[source]]
label = "s"
volume = "/vol"
device = "UUID=abc"
[[source.subvolumes]]
name = "@"
[[target]]
label = "primary-22tb"
serial = "ZXA1R71M"
mount = "$CW/not-mounted/primary"
role = "primary"
[target.retention]
daily = 7
[[target]]
label = "system-recovery-A-2tb"
serial = "ZK208Q77"
mount = "$CW/not-mounted/a"
mount_uuid = "$UUID_A"
role = "mirror"
[target.retention]
daily = 7
[email]
enabled = false
[gui]
enabled = false
EOF
    }
    # An OS root whose btrbk.timer is enabled: $1 the root, $2 "config" to
    # give it btrbk's config (then btrbk will run at boot; without, it stops
    # at once, and the verdict is no).
    contract_root() {
        mkdir -p "$1/etc/systemd/system/timers.target.wants" "$1/usr/lib/systemd/system" "$1/usr/lib/modules/7.2.8-1-cachyos"
        printf 'PRETTY_NAME="Fixture"\n' >"$1/etc/os-release"
        printf '[Service]\nType=oneshot\nExecStart=/usr/bin/btrbk run\n' >"$1/usr/lib/systemd/system/btrbk.service"
        printf '[Timer]\nOnCalendar=daily\nPersistent=true\n' >"$1/usr/lib/systemd/system/btrbk.timer"
        ln -s /nonexistent/x/btrbk.timer "$1/etc/systemd/system/timers.target.wants/btrbk.timer"
        if [[ "${2:-}" == config ]]; then
            mkdir -p "$1/etc/btrbk"
            echo "volume /x" >"$1/etc/btrbk/btrbk.conf"
        fi
    }
    # The record, as above: $1 the root, $2 the seeded mount_uuid as
    # uuid_field takes it (ABSENT: as a writer before df0 left it). Sets
    # C_STATUS_RC; the record is $STATE_FILE.
    contract_record() {
        local entry os
        # Its exit status is the reading's verdict (1: stale, which a fixture
        # root without a pacman log is), not whether it printed the entry.
        entry="$("$REAL_BTRDASD" --json recovery-os inspect --root "$1" --label system-recovery-A-2tb \
            --config "$CW/config.toml" 2>&1)" || :
        os="$(jq -c '.os' <<<"$entry" 2>&1)" || os=""
        printf '{"schema_version":3,"drives":{"system-recovery-A-2tb":{"checked_epoch":%s,%s"os":%s,"error":null,"seeded":true}}}\n' \
            "$(($(date +%s) - 3600))" "$(uuid_field system-recovery-A-2tb "$2")" "${os:-null}" >"$STATE_FILE"
        C_STATUS_RC=0
        "$REAL_BTRDASD" recovery-os status --config "$CW/config.toml" --state-file "$STATE_FILE" >/dev/null 2>&1 || C_STATUS_RC=$?
    }
    contract_get() { # contract_get <jq filter on the label's record>
        jq -c --arg l system-recovery-A-2tb ".drives[\$l] | $1" "$STATE_FILE" 2>&1 || :
    }

    fixture
    CW="$T/contract"
    mkdir -p "$CW"
    contract_config
    contract_root "$CW/no"
    contract_root "$CW/will" config

    contract_record "$CW/no" ""
    check "contract, no: the real status rewrote the record (exit 0)" "$C_STATUS_RC" "0"
    check "contract, no: through btrdasd's own types (the seeded key gone)" "$(contract_get 'has("seeded")')" "false"
    check "contract, no: the verdict the binary read" "$(contract_get '.os.btrbk_at_boot.verdict')" '"no"'
    check "contract, no: the filesystem kept" "$(contract_get '.mount_uuid')" "\"$UUID_A\""
    check "contract, no: schema 4" "$(jq -c '.schema_version' "$STATE_FILE")" "4"
    run_driver session A --dry-run
    check "contract, no: the driver goes on" "$RC" "0"
    has "contract, no: the driver read the binary's verdict" "$OUT" "btrbk at boot  no"
    has "contract, no: and its reason" "$OUT" "    - btrbk.timer starts btrbk.service, which runs btrbk at its next scheduled time after boot, but /etc/btrbk.conf and /etc/btrbk/btrbk.conf are absent, so it stops at once"
    has "contract, no: and its filesystem" "$OUT" "  filesystem     $UUID_A (the mount_uuid of system-recovery-A-2tb)"

    contract_record "$CW/will" ""
    rm -f "$S/flock.calls" "$S/holder.calls" # the dry run above took and gave back both
    check "contract, will: the real status rewrote the record (exit 0)" "$C_STATUS_RC" "0"
    check "contract, will: the verdict the binary read" "$(contract_get '.os.btrbk_at_boot.verdict')" '"will"'
    run_driver session A --dry-run
    check "contract, will: the driver goes on attended, with the banner" "$RC" "0"
    has "contract, will: the banner" "$OUT" "this OS runs btrbk at boot; the guard is stopping it now"
    run_driver session A --dry-run --unattended
    check "contract, will: the driver refuses unattended" "$RC" "1"
    has "contract, will: because a will drive is attended only" "$OUT" "a \"will\" drive is updated attended only"

    # The guest agent as the real binary reads it (schema 4): installed the
    # way Arch's package lays it out, its udev rule starting it.
    contract_root "$CW/agent"
    mkdir -p "$CW/agent/var/lib/pacman/local/qemu-guest-agent-11.0.0-1" "$CW/agent/usr/lib/udev/rules.d"
    printf '%%NAME%%\nqemu-guest-agent\n\n%%VERSION%%\n11.0.0-1\n' >"$CW/agent/var/lib/pacman/local/qemu-guest-agent-11.0.0-1/desc"
    printf '[Service]\nExecStart=-/usr/bin/qemu-ga\n[Install]\n' >"$CW/agent/usr/lib/systemd/system/qemu-guest-agent.service"
    printf 'SUBSYSTEM=="virtio-ports", ENV{SYSTEMD_WANTS}="qemu-guest-agent.service"\n' >"$CW/agent/usr/lib/udev/rules.d/99-qemu-guest-agent.rules"
    contract_record "$CW/agent" ""
    check "contract, agent: the real status rewrote the record (exit 0)" "$C_STATUS_RC" "0"
    check "contract, agent: the binary read it installed and started at boot" "$(contract_get '.os.guest_agent | [.state, .installed, .enabled]')" '["read",true,true]'
    run_driver session A --dry-run --unattended
    check "contract, agent: the driver goes on unattended" "$RC" "0"
    has "contract, agent: the driver read the binary's reading" "$OUT" "guest agent    read true true (started by usr/lib/udev/rules.d/99-qemu-guest-agent.rules"
    contract_record "$CW/no" ""
    check "contract, no agent: the binary read it unknown (no pacman database)" "$(contract_get '.os.guest_agent.state')" '"unreadable"'
    run_driver session A --dry-run --unattended
    check "contract, no agent: the driver refuses unattended" "$RC" "1"
    has "contract, no agent: because the agent is not known to run" "$OUT" "an unattended session needs the recovery OS's QEMU guest agent installed and started at boot"

    contract_record "$CW/no" ABSENT
    check "contract, a record from before df0: rewritten (exit 0)" "$C_STATUS_RC" "0"
    check "contract, a record from before df0: written back naming no filesystem" "$(contract_get 'has("mount_uuid"), .mount_uuid')" $'true\nnull'
    run_driver session A --dry-run --accept-boot-record-risk
    check "contract, a record from before df0: the driver refuses, overridden or not" "$RC" "1"
    has "contract, a record from before df0: because it names no filesystem" "$OUT" "does not name the filesystem it was read from"

    contract_record "$CW/no" '"ffffffff-0000-4000-8000-000000000000"'
    check "contract, a record of another filesystem: kept by the binary" "$(contract_get '.mount_uuid')" '"ffffffff-0000-4000-8000-000000000000"'
    run_driver session A --dry-run --accept-boot-record-risk
    check "contract, a record of another filesystem: the driver refuses" "$RC" "1"
    has "contract, a record of another filesystem: says which" "$OUT" "was read from filesystem ffffffff-0000-4000-8000-000000000000, not $UUID_A"
fi

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
matches "session: recorded before the OS was resumed" "$(grep '^system-recovery-A-2tb ' "$S/sessions.at_resume")" "^system-recovery-A-2tb [0-9]+$"
check "session: ...and not before it started, paused (dmxt)" "$(grep -c '^system-recovery-A-2tb ' "$S/sessions.at_start" || :)" "0"
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
lacks "session time cannot be recorded: the OS never ran" "$(events)" "virsh resume"
has "session time cannot be recorded: torn down while still paused" "$(events)" "virsh destroy"
check "session time cannot be recorded: lock free" "$(lock_state)" "free"

fixture
touch "$S/start_fail"
run_driver session A
check "start failed: refused" "$RC" "1"
check "start failed: no session time -- the OS never ran (dmxt)" "$(file "$SESSIONS")" ""
rm -f "$S/start_fail"
run_driver session A --dry-run
check "start failed: the retry is admitted (dmxt)" "$RC" "0"
lacks "start failed: ...not refused for a session that never ran" "$OUT" "before this drive's last VM session ended"

# A managed-save image refuses the start; the OS never ran, so the retry the
# refusal promises is admitted (dmxt).
fixture
touch "$S/managed_save_at_attach"
run_driver session A
check "managed save at the start: refused" "$RC" "1"
check "managed save at the start: no session time (dmxt)" "$(file "$SESSIONS")" ""
rm -f "$S/managed_save_at_attach"
run_driver session A --dry-run
check "managed save at the start: the retry is admitted (dmxt)" "$RC" "0"

# Started paused without the guard and torn down: nothing ran, and the retry
# never_ran_text promises is admitted (dmxt).
fixture
touch "$S/live_drops_guard"
run_driver session A
check "destroyed paused: exit 1" "$RC" "1"
has "destroyed paused: a retry is safe" "$OUT" "A retry is safe: run the session again"
check "destroyed paused: no session time (dmxt)" "$(file "$SESSIONS")" ""
rm -f "$S/live_drops_guard"
run_driver session A --dry-run
check "destroyed paused: the retry is admitted (dmxt)" "$RC" "0"

echo "--- the session guard: btrbk cannot run in the booted recovery OS (bd 0zm)"
# A report line, as a recovery OS whose guard holds writes it: $1 the body,
# $2 its boot (BOOT_A by default).
# The stub's guest reports this boot, as the test's own lines do.
BOOT_A=aaaaaaaa-1111-4222-8333-444444444444
ENG4="das-vm-guard engaged 4 masks btrbk.service,btrbk.timer,cronie.service,crond.service"
LIFT4="das-vm-guard lifted 4 masks btrbk.service,btrbk.timer,cronie.service,crond.service"
BOOT_B=bbbbbbbb-1111-4222-8333-444444444444
# $3 its number in that boot (1 by default).
line() { printf '%s seq %s boot %s\n' "$1" "${3:-1}" "${2:-$BOOT_A}"; }
# A guest that keeps running until asked to stop, and stops when asked.
keeps_running() {
    for ((i = 0; i < 400; i++)); do echo running; done >"$S/states.running"
    printf 'shut off\n' >"$S/states.after_shutdown"
}

fixture
run_driver session A
check "guard: a session ends" "$RC" "0"
x="$S/defined.at_start.xml"
has "guard: the VM booted with SMBIOS strings" "$(file "$x")" "<sysinfo type='smbios'>"
has "guard: ...read from its sysinfo" "$(file "$x")" "<smbios mode='sysinfo'/>"
has "guard: the guard unit, a credential" "$(file "$x")" "<entry>io.systemd.credential.binary:systemd.extra-unit.das-vm-guard.service="
has "guard: the reporter unit, a credential" "$(file "$x")" "<entry>io.systemd.credential.binary:systemd.extra-unit.das-vm-guard-report.service="
has "guard: pulled in by sysinit.target" "$(file "$x")" "<entry>io.systemd.credential.binary:systemd.unit-dropin.sysinit.target~das-vm-guard="
has "guard: the refusing btrbk, a credential" "$(file "$x")" "<entry>io.systemd.credential.binary:das-vm-guard.btrbk="
for u in btrbk.service btrbk.timer cronie.service crond.service; do
    has "guard: $u masked" "$(file "$x")" "<entry>io.systemd.credential:systemd.extra-unit.$u=</entry>"
    check "guard: $u also held by a never-true condition, sorting last" \
        "$(credential "$x" "systemd.unit-dropin.$u~zzzzzzzz-das-vm-guard")" $'[Unit]\nConditionPathExists=/dev/null/das-vm-guard'
done
check "guard: those four masks, no more" "$(grep -c '<entry>io.systemd.credential:systemd.extra-unit\.' "$x")" "4"
check "guard: and four conditions" "$(grep -c '<entry>io.systemd.credential.binary:systemd.unit-dropin\.[^<]*~zzzzzzzz-das-vm-guard=' "$x")" "4"
check "guard: 4 units and drop-ins of its own, 2 strings per mask" "$(grep -c '<entry>' "$x")" "12"
has "guard: a confirmation channel" "$(file "$x")" "<target type='virtio' name='org.dasbackup.guard'/>"
has "guard: ...into a file" "$(file "$x")" "<channel type='file'>"
has "guard: ...in the session's own root-only directory" "$(file "$x")" "<source path='$STATE/system-recovery-A-2tb.guard'/>"
if command -v virt-xml-validate >/dev/null; then
    check "guard: the definition it boots is valid libvirt XML" "$(virt-xml-validate "$x" domain 2>&1 | tail -n 1)" "$x validates"
fi
g="$(credential "$x" systemd.extra-unit.das-vm-guard.service)"
for want in "DefaultDependencies=no" "ConditionPathExists=!/etc/initrd-release" "After=systemd-remount-fs.service" \
    "Before=systemd-udev-trigger.service local-fs-pre.target sysinit.target" "Type=oneshot" "RemainAfterExit=yes" \
    "TimeoutStartSec=60" "ImportCredential=das-vm-guard.btrbk" \
    "install -D -m 0755 \"\$\$CREDENTIALS_DIRECTORY/das-vm-guard.btrbk\" /run/das-vm-guard/btrbk" \
    "for p in /usr/bin/btrbk /usr/local/bin/btrbk /usr/local/sbin/btrbk /usr/sbin/btrbk /bin/btrbk /sbin/btrbk;" \
    "/usr/bin/mount --bind /run/das-vm-guard/btrbk" "> /dev/console" "> /run/issue.d/das-vm-guard.issue"; do
    has "guard unit: $want" "$g" "$want"
done
lacks "guard unit: not ordered after local-fs.target (udev coldplug runs before that)" "$g" "After=local-fs.target"
has "guard unit: announces itself on /dev/kmsg" "$g" "ExecStart=-/usr/bin/sh -c 'echo \"das-vm-guard: btrbk cannot run in this VM session; before pacman: systemctl stop das-vm-guard; after it: pacman -Qkk btrbk must find 0 altered files\" > /dev/kmsg'"
for want in "das-vm-guard: btrbk cannot run in this VM session (DAS recovery OS updater)." \
    "  To update, follow the first update in the disaster recovery guide. In short:" \
    "  1. lift it:    systemctl stop das-vm-guard   (if it failed: umount /usr/bin/btrbk /usr/local/bin/btrbk /usr/local/sbin/btrbk /usr/sbin/btrbk /bin/btrbk /sbin/btrbk)" \
    "  2. keyrings:   pacman -Sy archlinux-keyring cachyos-keyring" \
    "  3. drivers:    pin both worlds into the initramfs first (the guide, step 2)" \
    "  4. upgrade:    pacman -Su" \
    "  5. check:      pacman -Qkk btrbk   must find 0 altered files" \
    "  6. re-engage:  systemctl start das-vm-guard   (lifted, pacman hooks and units not masked can run btrbk)" \
    "  7. reboot, check uname -r, then systemctl poweroff"; do
    check "guard unit: at the console and the login prompt: ${want# }" "$(grep -c -F "echo \"$want\";" <<<"$g")" "2"
done
check "guard unit: stopping it lifts it, in reverse" "$(grep '^ExecStop=' <<<"$g")" \
    "ExecStop=-/usr/bin/umount /sbin/btrbk
ExecStop=-/usr/bin/umount /bin/btrbk
ExecStop=-/usr/bin/umount /usr/sbin/btrbk
ExecStop=-/usr/bin/umount /usr/local/sbin/btrbk
ExecStop=-/usr/bin/umount /usr/local/bin/btrbk
ExecStop=-/usr/bin/umount /usr/bin/btrbk
ExecStop=-/usr/bin/rm -f /run/issue.d/das-vm-guard.issue
ExecStop=-/usr/bin/sh -c 'echo \"das-vm-guard: lifted: btrbk can run again; its units and cron stay masked until the VM powers off\" > /dev/kmsg'"
has "guard unit: the console write cannot hold up the boot" "$g" "ExecStart=-/usr/bin/timeout 5 /usr/bin/sh -c '{ echo;"
check "guard: the drop-in, exactly" "$(credential "$x" 'systemd.unit-dropin.sysinit.target~das-vm-guard')" $'[Unit]\nWants=das-vm-guard.service das-vm-guard-report.service'
r="$(credential "$x" systemd.extra-unit.das-vm-guard-report.service)"
for want in "After=das-vm-guard.service" "ConditionPathExists=!/etc/initrd-release" "/dev/virtio-ports/org.dasbackup.guard" \
    "for u in btrbk.service btrbk.timer cronie.service crond.service;" "LoadState" "FragmentPath" "DropInPaths" \
    "masked) ;;" "/proc/sys/kernel/random/boot_id" "sleep 60; done" "findmnt -rn -o TARGET" "Type=simple" \
    "Restart=on-failure" "RestartSec=5" "StartLimitIntervalSec=1h" "StartLimitBurst=4" "/run/das-vm-guard/report.state"; do
    has "reporter unit: $want" "$r" "$want"
done
lacks "reporter unit: ordered before nothing" "$r" "Before="
lacks "reporter unit: runs as long as the VM does" "$r" "RuntimeMaxSec="
stub="$(credential "$x" das-vm-guard.btrbk)"
check "guard: the refusing btrbk is shell" "$(sh -n -c "$stub" 2>&1 && echo ok)" "ok"
has "guard: the refusing btrbk says so on /dev/kmsg" "$stub" "> /dev/kmsg"
has "guard: ...and exits nonzero" "$stub" "exit 1"
has "guard: confirmed, and said" "$OUT" "the session guard is engaged: $ENG4"
has "guard: in the summary" "$OUT" "Guard         engaged -- $ENG4 (1 boot; lifted 0 times)"
has "guard: the operator told what to do inside" "$OUT" "inside it, before updating: systemctl stop das-vm-guard; after: pacman -Qkk btrbk must find 0 altered files, then systemctl start das-vm-guard"
check "guard: defined before the attach and the start, the template again after the detach" \
    "$(calls_in_order "define --validate $STATE/system-recovery-A-2tb.domain.xml" "attach-device" "start --paused --force-boot recovery-os-updater-system-recovery-A-2tb" "detach-disk" "define --validate $STATE/system-recovery-A-2tb.plain.xml")" "yes"
check "guard: started paused, read back while paused, then resumed" \
    "$(calls_in_order "start --paused --force-boot recovery-os-updater-system-recovery-A-2tb" "dumpxml recovery-os-updater-system-recovery-A-2tb" "resume recovery-os-updater-system-recovery-A-2tb")" "yes"
lacks "guard: out of the definition afterwards (strings)" "$(file "$S/defined.xml")" "oemStrings"
lacks "guard: ...(channel)" "$(file "$S/defined.xml")" "org.dasbackup.guard"
lacks "guard: ...(smbios)" "$(file "$S/defined.xml")" "smbios"
check "guard: nothing of it left in the session's directory" "$(ls -A "$STATE")" ""
check "guard: never destroyed" "$(file "$S/forbidden")" ""

fixture
GUARD_SECS=default run_driver session A --dry-run
has "guard: the deadline without the knob is 15 minutes" "$OUT" "the session guard must report within 0h 15m 00s"

# What the record names is masked too -- by name, and only names that are
# unit names; nothing else from the record reaches a credential. A "no"
# record, so the names it will not mask cost a warning, not the session.
fixture
write_state 3 "$(record_with_runners system-recovery-A-2tb no \
    '["btrbk-hourly.timer starts btrbk-hourly.service, which runs btrbk at its next scheduled time after boot, its config unknown: a test","mkinitcpio-generate-shutdown-ramfs.service may run btrbk: a script not read","x=y.service may run btrbk: a test","more than 4096 units start at boot: the rest were not read"]' \
    '[{"source":"spike-btrbk-at-boot.service","via":"multi-user.target"},{"source":"etc/cron.d/backup","via":"cronie.service"},{"source":"a=b.service","via":"x y.timer"},{"source":"das-vm-guard.service","via":null},{"source":"sysinit.target","via":null},{"source":"../evil.service","via":"das-vm-guard-report.service"},{"source":"evil.service\n<entry>io.systemd.credential:systemd.extra-unit.sshd.service=</entry>","via":"multi-user.target"},{"source":"evil\u001b[31m.service","via":null},{"source":"etc/cron.d/other","via":"fcron.service"},{"source":"backup@.service","via":null},{"source":"backup@daily.service","via":"backup.timer"},{"source":"etc/cron.d/third","via":"bcron.service"},{"source":"tail.service=x","via":null},{"source":"systemd-backlight@backlight:acpi_video0.service","via":null},{"source":"é.service","via":null}]' \
    fcron.service anacron.timer)"
LC_ALL=en_US.UTF-8 run_driver session A
check "guard, what the record names: a session ends, flagged" "$RC" "5"
check "guard, what the record names: virsh runs in the C locale (its answers in English)" "$(sort -u "$S/virsh.lc_all")" "C"
x="$S/defined.at_start.xml"
masks="btrbk.service btrbk.timer cronie.service crond.service anacron.timer backup@daily.service bcron.service btrbk-hourly.service btrbk-hourly.timer fcron.service mkinitcpio-generate-shutdown-ramfs.service spike-btrbk-at-boot.service"
for u in $masks; do
    has "guard, what the record names: $u masked" "$(file "$x")" "<entry>io.systemd.credential:systemd.extra-unit.$u=</entry>"
done
check "guard, what the record names: twelve masks, no more" "$(grep -c '<entry>io.systemd.credential:systemd.extra-unit\.' "$x")" "12"
for bad in "a=b.service" "x y.timer" "x=y.service" "evil." "evil?" "sshd.service" "multi-user.target=" "sysinit.target=" \
    "systemd.extra-unit.das-vm-guard.service=</entry>" "systemd.extra-unit.das-vm-guard-report.service=</entry>" "cron.d" \
    "backup@.service" "tail.service" "acpi_video0" $'\xc3\xa9'; do
    lacks "guard, a name that is no plain unit never reaches a credential: $bad" "$(file "$x")" "$bad"
done
for said in "a=b.service" "x=y.service" "das-vm-guard.service" "das-vm-guard-report.service" "sysinit.target" "multi-user.target" "evil?[31m.service" "backup@.service" "tail.service=x" "systemd-backlight@backlight:acpi_video0.service" "??.service"; do
    has "guard, a name it will not mask, said: $said" "$OUT" "not masked: '$said'"
done
lacks "guard, a name it will not mask: no control character reaches the terminal" "$OUT" $'\e'
has "guard, what the record names: the reporter checks every mask" "$(credential "$x" systemd.extra-unit.das-vm-guard-report.service)" "for u in ${masks};"
has "guard, what the record names: confirmed with all twelve" "$OUT" "das-vm-guard engaged 12 masks ${masks// /,}"
has "guard, what the record names: what it would not mask, in the summary" "$OUT" "Warnings      the boot record names units the guard cannot mask:"

# M-2: on a "may" record, a named runner the guard cannot mask refuses the
# session before anything is taken -- whatever the reason it cannot.
fixture
write_state 3 "$(record_with_runners system-recovery-A-2tb may '["a test"]' '[{"source":"a=b.service","via":null}]')"
run_driver session A --dry-run
check "guard, may, a runner it cannot mask: refused" "$RC" "1"
has "guard, may, a runner it cannot mask: says which" "$OUT" "REFUSED: the boot record says btrbk may run when this OS boots, and names units the session guard cannot mask: 'a=b.service'"
check "guard, may, a runner it cannot mask: nothing locked" "$(file "$S/flock.calls")" ""

fixture
runners='['
for ((i = 1; i <= 70; i++)); do runners+="$(printf '{"source":"r%02d.service","via":null},' "$i")"; done
write_state 3 "$(record_with_runners system-recovery-A-2tb may '["a test"]' "${runners%,}]")"
run_driver session A
check "guard, may, 70 runners: refused" "$RC" "1"
has "guard, may, 70 runners: names the ones past the limit" "$OUT" "names units the session guard cannot mask: r61.service r62.service r63.service r64.service r65.service r66.service r67.service r68.service r69.service r70.service (it masks at most 64)"
lacks "guard, may, 70 runners: never booted" "$(events)" "virsh start"

fixture
write_state 3 "$(record_with_runners system-recovery-A-2tb no '["a test"]' "${runners%,}]")"
run_driver session A
check "guard, no, 70 runners: a session ends, flagged" "$RC" "5"
check "guard, no, 70 runners: 64 masks in all" "$(grep -c '<entry>io.systemd.credential:systemd.extra-unit\.' "$S/defined.at_start.xml")" "64"
has "guard, no, 70 runners: the first 60 of them, in order" "$(file "$S/defined.at_start.xml")" "systemd.extra-unit.r60.service="
has "guard, no, 70 runners: the rest in the summary" "$OUT" "Warnings      the boot record names units the guard cannot mask: r61.service r62.service"
# The SMBIOS budget at the limit: OEM strings are counted in one byte.
entries=$(grep -c '<entry>' "$S/defined.at_start.xml" 2>/dev/null) || entries=0
check "guard, 64 masks: strings within SMBIOS's 255" "$((entries <= 255))" "1"
bytes=$(sed -n 's|.*<entry>\(.*\)</entry>.*|\1|p' "$S/defined.at_start.xml" 2>/dev/null | wc -c) || bytes=0
check "guard, 64 masks: under 64 KiB of strings ($entries strings, $bytes bytes)" "$((bytes < 65536))" "1"

# I-1: the guard is checked in the started domain while it is paused, before
# it runs. Without it there, the domain is destroyed -- it never ran a single
# instruction -- and nothing boots.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/live_drops_guard"
keeps_running
run_driver session A
check "guard not in the started domain, may: refused" "$RC" "1"
has "guard not in the started domain, may: said" "$OUT" "the started domain does not carry the session guard (missing: io.systemd.credential.binary:systemd.extra-unit.das-vm-guard.service"
has "guard not in the started domain, may: destroyed while paused" "$(events)" "virsh start|virsh destroy|virsh detach|virsh define|holder released|"
lacks "guard not in the started domain, may: never resumed" "$(events)" "virsh resume"
has "guard not in the started domain, may: says it never ran" "$OUT" "it was destroyed while still paused, before it ran a single instruction"
check "guard not in the started domain, may: no forbidden destroy" "$(file "$S/forbidden")" ""
check "guard not in the started domain, may: lock free" "$(lock_state)" "free"

fixture
touch "$S/live_drops_guard"
run_driver session A
check "guard not in the started domain, no record: refused too" "$RC" "1"
lacks "guard not in the started domain, no record: never resumed" "$(events)" "virsh resume"

# 1518 (3): the started domain's definition cannot be read.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/dumpxml_live_fail"
run_driver session A
check "the started domain cannot be read: refused" "$RC" "1"
has "the started domain cannot be read: said" "$OUT" "cannot read the started domain's definition"
has "the started domain cannot be read: destroyed while paused" "$(events)" "virsh start|virsh destroy|"
lacks "the started domain cannot be read: never resumed" "$(events)" "virsh resume"

# virsh start says it failed, though the domain started (paused): it never
# ran, so it is destroyed, and the session ends.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/start_error_but_started"
run_driver session A
check "start fails though the domain started: exit 1" "$RC" "1"
has "start fails though the domain started: destroyed while paused" "$(events)" "virsh start|virsh destroy|virsh detach|"
lacks "start fails though the domain started: never resumed" "$(events)" "virsh resume"
check "start fails though the domain started: no forbidden destroy" "$(file "$S/forbidden")" ""
check "start fails though the domain started: lock free" "$(lock_state)" "free"

# Interrupted just as the recovery OS powered off: its report is judged before
# the disk -- and the report with the guard -- is given back.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
printf 'shut off\n' >"$S/states.running"
POLL=2 GUARD_SECS=300 run_interrupted session A
check "interrupted as it powers off, no report, may: exit 6" "$RC" "6"
has "interrupted as it powers off, no report, may: said" "$OUT" "but the session guard did not confirm on a \"may\" record (the recovery OS powered off without reporting)"
check "interrupted as it powers off, no report, may: the disk given back" "$(file "$S/attached.xml")" ""
check "interrupted as it powers off, no report, may: lock free" "$(lock_state)" "free"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
printf 'shut off\n' >"$S/states.running"
POLL=2 GUARD_SECS=300 run_interrupted session A
check "interrupted as it powers off, engaged: exit 1" "$RC" "1"
has "interrupted as it powers off, engaged: said" "$OUT" "session guard: engaged -- $ENG4 (1 boot; lifted 0 times)"

# The destroy of the paused domain fails: kept, never said to be destroyed,
# told it never ran and how to end it; session-end then finds nothing to judge.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/live_drops_guard" "$S/destroy_fail"
run_driver session A
check "guard missing, the destroy fails: kept (exit 3)" "$RC" "3"
lacks "guard missing, the destroy fails: never said to be destroyed" "$OUT" "it was destroyed while still paused"
has "guard missing, the destroy fails: said" "$OUT" "and it could not be destroyed: it is still paused, and has run nothing"
has "guard missing, the destroy fails: says it never ran" "$OUT" "was started paused and never resumed: it has not run a single instruction"
has "guard missing, the destroy fails: how to end it, once it reads paused and never run" "$OUT" "virsh --connect qemu:///system domstats --state --vcpu recovery-os-updater-system-recovery-A-2tb"
has "guard missing, the destroy fails: what the read must say" "$OUT" "(must say state.state=3, and vcpu.N.time=0 for every vCPU)"
lacks "guard missing, the destroy fails: never resumed" "$(events)" "virsh resume"
check "guard missing, the destroy fails: no forbidden destroy" "$(file "$S/forbidden")" ""
rm -f "$S/destroy_fail"
printf 'shut off\n' >"$S/states"
run_driver session-end A
check "session-end, started but never resumed: exit 0" "$RC" "0"
has "session-end, started but never resumed: nothing to judge" "$OUT" "session guard: never ran (started paused, never resumed): nothing to judge"

# A resume that fails: it may have run, so it is never destroyed.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/resume_fail"
run_driver session A
check "resume fails: the session is kept" "$RC" "3"
lacks "resume fails: never destroyed" "$(file "$S/virsh.calls")" "destroy"
check "resume fails: no forbidden destroy" "$(file "$S/forbidden")" ""
has "resume fails: the guard not judged, said" "$OUT" "THE SESSION GUARD HAS NOT BEEN JUDGED"

# I-1: a stop for the guard is ACPI, re-sent until the recovery OS is off.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
keeps_running
echo 2 >"$S/shutdown_ignore"
DRIVER_TIMEOUT=30 GRACE=10 RESEND=1 GUARD_SECS=1 run_driver session A
check "guard silent, may, the first requests dropped: exit 6" "$RC" "6"
check "guard silent, may, the first requests dropped: asked three times" "$(grep -c '^shutdown' "$S/virsh.calls")" "3"
check "guard silent, may, the first requests dropped: never destroyed" "$(file "$S/forbidden")" ""

# ...and when it never goes: kept, and told plainly what to do -- not to let
# an update finish.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
for ((i = 0; i < 400; i++)); do echo running; done >"$S/states.running"
DRIVER_TIMEOUT=30 GRACE=3 RESEND=1 GUARD_SECS=1 run_driver session A
check "guard silent, may, the guest ignores every request: kept (exit 3)" "$RC" "3"
matches "guard silent, may, the guest ignores every request: asked again and again" "$(grep -c '^shutdown' "$S/virsh.calls")" "^[2-9]$"
check "guard silent, may, the guest ignores every request: never destroyed" "$(file "$S/forbidden")" ""
has "guard silent, may, the guest ignores every request: says it is unguarded" "$OUT" "THE RECOVERY OS IS RUNNING WITHOUT A CONFIRMED SESSION GUARD"
has "guard silent, may, the guest ignores every request: says to power it off now" "$OUT" "Power it off now"
lacks "guard silent, may, the guest ignores every request: never says to let the update finish" "$OUT" "Let the update finish"

# The recovery OS stays silent. On a "may" record: shut down (never
# destroyed), the disk given back, exit 6.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
keeps_running
DRIVER_TIMEOUT=20 GUARD_SECS=1 run_driver session A
check "guard silent, may: exit 6" "$RC" "6"
has "guard silent, may: said" "$OUT" "the session guard did not confirm: no report from the recovery OS within 0h 00m 01s of its start"
has "guard silent, may: asked to shut down" "$(events)" "virsh start|virsh resume|virsh shutdown|virsh detach|virsh define|holder released|"
check "guard silent, may: asked once" "$(grep -c '^shutdown' "$S/virsh.calls")" "1"
check "guard silent, may: never destroyed" "$(file "$S/forbidden")" ""
has "guard silent, may: in the summary" "$OUT" "Guard         NOT confirmed: no report from the recovery OS"
has "guard silent, may: a warning in the summary" "$OUT" "Warnings      the session guard did not confirm on a \"may\" record, so the recovery OS was asked to shut down"
check "guard silent, may: the disk given back" "$(file "$S/attached.xml")" ""
check "guard silent, may: lock free" "$(lock_state)" "free"
lacks "guard silent, may: out of the definition" "$(file "$S/defined.xml")" "oemStrings"

fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
keeps_running
DRIVER_TIMEOUT=20 GUARD_SECS=3 run_driver session A
check "guard silent, may, 3s: exit 6" "$RC" "6"
matches "guard silent, may, 3s: not before the deadline" "$OUT" "VM ran        0h 00m 0[3-9]s"

# On a "no" record: a loud warning, the session goes on, exit 5.
fixture
touch "$S/guard_silent"
for ((i = 0; i < 60; i++)); do echo running; done >"$S/states.running"
echo "shut off" >>"$S/states.running"
GUARD_SECS=1 run_driver session A
check "guard silent, no record: exit 5" "$RC" "5"
lacks "guard silent, no record: no shutdown" "$(events)" "virsh shutdown"
has "guard silent, no record: a warning in the summary" "$OUT" "Warnings      the session guard did not confirm (no report from the recovery OS"

# A "will" record, attended (decision 3), is no safer than "may".
fixture
write_state 3 "$(record_json system-recovery-A-2tb will)"
touch "$S/guard_silent"
keeps_running
DRIVER_TIMEOUT=20 GUARD_SECS=1 run_driver session A
check "guard silent, will: exit 6" "$RC" "6"
has "guard silent, will: shut down" "$(events)" "virsh shutdown"

# Off before it reported: nothing to shut down, the same verdict.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
GUARD_SECS=300 run_driver session A
check "guard, off before it reported, may: exit 6" "$RC" "6"
has "guard, off before it reported, may: said" "$OUT" "the recovery OS powered off without reporting"
lacks "guard, off before it reported, may: nothing to shut down" "$(events)" "virsh shutdown"

fixture
touch "$S/guard_silent"
GUARD_SECS=300 run_driver session A
check "guard, off before it reported, no record: exit 5" "$RC" "5"

# Every line is judged (1518 1), not only the first.
REPLY="$(mktemp)"
ROOTS+=("$REPLY")
judged() { # $1 the case, $2 the expected exit, $3 the expected words, $4 GUARD_SECS; the reply in $REPLY
    fixture
    write_state 3 "$(record_json system-recovery-A-2tb may)"
    cp "$REPLY" "$S/guard_reply"
    keeps_running
    DRIVER_TIMEOUT=20 GUARD_SECS="${4:-300}" run_driver session A
    check "guard, $1: exit $2" "$RC" "$2"
    has "guard, $1: said" "$OUT" "$3"
}
line "das-vm-guard NOT engaged: btrbk.timer is not masked (loaded);" >"$REPLY"
judged "not engaged" 6 "the recovery OS reports it NOT engaged: btrbk.timer is not masked (loaded);"
{ line "$ENG4"; line "das-vm-guard NOT engaged: das-vm-guard.service is failed;" "$BOOT_A" 2; } >"$REPLY"
judged "engaged, then not" 6 "the recovery OS reports it NOT engaged: das-vm-guard.service is failed;"
{ line "$ENG4"; line "$ENG4" "$BOOT_B"; line "das-vm-guard NOT engaged: btrbk.service is not masked (loaded);" "$BOOT_B" 2; } >"$REPLY"
judged "a second boot not engaged" 6 "the recovery OS reports it NOT engaged: btrbk.service is not masked (loaded);"
{ line "$ENG4"; line "$LIFT4" "$BOOT_B"; } >"$REPLY"
judged "a second boot that begins lifted" 6 "boot $BOOT_B began without the guard engaged"
{ line "$ENG4"; line "das-vm-guard engaged 3 masks btrbk.service,btrbk.timer,cronie.service" "$BOOT_A" 2; } >"$REPLY"
judged "a later line of another kind" 6 "a report line that is no report: 'das-vm-guard engaged 3 masks btrbk.service,btrbk.timer,cronie.service seq 2 boot $BOOT_A'"
line "das-vm-guard engaged 3 masks btrbk.service,btrbk.timer,cronie.service" >"$REPLY"
judged "another line" 6 "a report line that is no report: 'das-vm-guard engaged 3 masks btrbk.service,btrbk.timer,cronie.service seq 1 boot $BOOT_A'"
printf '%s\n' "$ENG4" >"$REPLY"
judged "a line without its boot" 6 "a report line that is no report: '$ENG4'"
printf 'das-vm-guard engaged 4 masks btrbk.service,btrbk.timer,cronie.service,crond.service\e[2J' >"$REPLY"
judged "an unfinished line" 6 "(an unfinished line: 'das-vm-guard engaged 4 masks btrbk.service,btrbk.timer,cronie.service,crond.service?[2J')" 1
lacks "guard, an unfinished line: no control character reaches the terminal" "$OUT" $'\e'
printf '%s seq 1 boot %s' "$ENG4" "$BOOT_A" >"$REPLY"
judged "the right line unfinished" 6 "no report from the recovery OS within" 1
# Lifted for the update, re-engaged, rebooted engaged: all as it should be.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
{ line "$ENG4"; line "$LIFT4" "$BOOT_A" 2; line "$ENG4" "$BOOT_A" 3; line "$ENG4" "$BOOT_B"; } >"$S/guard_reply"
run_driver session A
check "guard, lifted and rebooted: exit 0" "$RC" "0"
has "guard, lifted and rebooted: the summary counts both" "$OUT" "Guard         engaged -- $ENG4 (2 boots; lifted 1 time)"

# Reports for longer than the deadline, one at every look: each one counts.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
for ((i = 0; i < 80; i++)); do echo running; done >"$S/states.running"
echo "shut off" >>"$S/states.running"
GUARD_SECS=2 run_driver session A
check "guard, reporting for longer than the deadline: exit 0" "$RC" "0"
matches "guard, reporting for longer than the deadline: it ran past it" "$OUT" "VM ran        0h (00m 0[3-9]|00m [1-5][0-9]|0[1-9]m [0-5][0-9])s"

# The report cut in place (not virtlogd's way): what it held unread is lost,
# and said; the lines after it still judged -- a warning, exit 5.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
for ((i = 0; i < 8; i++)); do echo running; done >"$S/states.running"
echo "shut off" >>"$S/states.running"
echo "3 truncate" >"$S/guest.script"
DRIVER_TIMEOUT=20 GUARD_SECS=300 run_driver session A
check "guard, the report cut in place: exit 5" "$RC" "5"
has "guard, the report cut in place: said" "$OUT" "the report was cut in place"
has "guard, the report cut in place: in the summary" "$OUT" "Warnings      the session guard's report: the report was cut in place"
lacks "guard, the report cut in place: no shutdown" "$(events)" "virsh shutdown"

# Fail closed: no guard, no boot; and it never outlives a failed session.
fixture
touch "$S/define_fail_guarded"
run_driver session A
check "guard cannot be defined: refused" "$RC" "1"
has "guard cannot be defined: says so" "$OUT" "REFUSED: cannot define the session guard into recovery-os-updater-system-recovery-A-2tb"
lacks "guard cannot be defined: never booted" "$(events)" "virsh start"
check "guard cannot be defined: nothing attached" "$(file "$S/attach.log")" ""
check "guard cannot be defined: lock free" "$(lock_state)" "free"
check "guard cannot be defined: nothing left in the session's directory" "$(ls -A "$STATE")" ""

fixture
touch "$S/define_drops_guard"
run_driver session A
check "guard not kept by libvirt: refused" "$RC" "1"
has "guard not kept by libvirt: says so" "$OUT" "recovery-os-updater-system-recovery-A-2tb's definition does not carry the session guard after defining it (missing: io.systemd.credential.binary:systemd.extra-unit.das-vm-guard.service"
lacks "guard not kept by libvirt: never booted" "$(events)" "virsh start"
has "guard not kept by libvirt: the template defined again after it" "$(events)" "virsh define|virsh define|holder released|"

fixture
sed -i "s|^  </os>|  </os>\n  </os>|" "$T/usr/lib/das-backup/libvirt/recovery-os-updater.xml"
run_driver session A
check "guard, a template of another shape: refused" "$RC" "1"
has "guard, a template of another shape: says so" "$OUT" "cannot add the session guard to $STATE/system-recovery-A-2tb.plain.xml"
lacks "guard, a template of another shape: nothing defined" "$(file "$S/virsh.calls")" "define"
check "guard, a template of another shape: nothing left in the session's directory" "$(ls -A "$STATE")" ""

fixture
sed -i "s|^  </os>|  </os>\n  <sysinfo type='smbios'/>|" "$T/usr/lib/das-backup/libvirt/recovery-os-updater.xml"
run_driver session A
check "guard, a template with SMBIOS strings of its own: refused" "$RC" "1"

fixture
mkdir -p "$STATE"
line "$ENG4" >"$STATE/system-recovery-A-2tb.guard"
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
GUARD_SECS=300 run_driver session A
check "guard, an old report left behind: never taken for this session's" "$RC" "6"
has "guard, an old report left behind: this session's has none" "$OUT" "the recovery OS powered off without reporting"
lacks "guard, an old report left behind: never said to be engaged" "$OUT" "the session guard is engaged"

fixture
touch "$S/attach_fail"
run_driver session A
check "guard, the attach fails: refused" "$RC" "1"
lacks "guard, the attach fails: out of the definition again" "$(file "$S/defined.xml")" "oemStrings"

fixture
touch "$S/start_fail"
run_driver session A
check "guard, the start fails: refused" "$RC" "1"
lacks "guard, the start fails: out of the definition again" "$(file "$S/defined.xml")" "oemStrings"

fixture
touch "$S/define_fail_plain"
run_driver session A
check "guard cannot be taken out: exit 5" "$RC" "5"
has "guard cannot be taken out: in the summary" "$OUT" "Warnings      the session guard is still in recovery-os-updater-system-recovery-A-2tb's definition (virsh define $STATE/system-recovery-A-2tb.plain.xml failed"
check "guard cannot be taken out: the disk given back all the same" "$(file "$S/attached.xml")" ""
check "guard cannot be taken out: lock free" "$(lock_state)" "free"

fixture
touch "$S/define_keeps_guard"
run_driver session A
check "guard still there after the template is defined: exit 5" "$RC" "5"
has "guard still there after the template is defined: said" "$OUT" "Warnings      the session guard is still in recovery-os-updater-system-recovery-A-2tb's definition (recovery-os-updater-system-recovery-A-2tb's definition still carries it after defining"

fixture
run_driver session A --dry-run
check "guard, dry run: exit 0" "$RC" "0"
lacks "guard, dry run: nothing defined" "$(file "$S/virsh.calls")" "define"
has "guard, dry run: the guard it would carry, shown" "$OUT" "session guard: a refusing btrbk bound over /usr/bin/btrbk, /usr/local/bin/btrbk, /usr/local/sbin/btrbk, /usr/sbin/btrbk, /bin/btrbk and /sbin/btrbk where present; masks btrbk.service, btrbk.timer, cronie.service, crond.service"

for knob in 0 12x; do
    GUARD_SECS=$knob run_driver status
    check "guard knob $knob: a usage error" "$RC" "2"
    has "guard knob $knob: says which" "$OUT" "_GUARD_SECS, _RESEND_SECS, _CLOCK_GAP_SECS, _UPDATE_SECS, _AGENT_SECS and _AGENT_GRACE_SECS take numbers above 0"
done
RESEND=0 run_driver status
check "resend knob 0: a usage error" "$RC" "2"
CLOCK_GAP=0 run_driver status
check "clock-gap knob 0: a usage error" "$RC" "2"

echo "--- I-2: a guard the driver never judged is judged by status and session-end"
# Interrupted before the recovery OS reported: the message says so.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
GUARD_SECS=300 run_interrupted session A
check "interrupted, not judged: kept (exit 3)" "$RC" "3"
has "interrupted, not judged: said" "$OUT" "THE SESSION GUARD HAS NOT BEEN JUDGED"
has "interrupted, not judged: where its report lands" "$OUT" "its report lands in $STATE/system-recovery-A-2tb.guard"
lacks "interrupted, not judged: never says to let the update finish" "$OUT" "Let the update finish"
check "interrupted, not judged: its session state root's only" "$(stat -c %a "$STATE/system-recovery-A-2tb.guard.state")" "600"
GUARD_SECS=300 run_driver status
check "status, a report not yet in: exit 0 within the deadline" "$RC" "0"
has "status, a report not yet in: said" "$OUT" "  Session guard   in the definition; no report yet"
echo "resumed=$(($(date +%s) - 1000))" >>"$STATE/system-recovery-A-2tb.guard.state" 2>/dev/null || :
GUARD_SECS=300 run_driver status
check "status, no report past the deadline: exit 6" "$RC" "6"
has "status, no report past the deadline: said" "$OUT" "  Session guard   in the definition; NOT engaged: no report from a recovery OS that ran"
line "das-vm-guard NOT engaged: btrbk.timer is not masked (loaded);" >"$STATE/system-recovery-A-2tb.guard" 2>/dev/null || :
run_driver status
check "status, a report NOT engaged: exit 6" "$RC" "6"
has "status, a report NOT engaged: said" "$OUT" "  Session guard   in the definition; NOT engaged: the recovery OS reports it NOT engaged: btrbk.timer is not masked (loaded);"
printf 'shut off\n' >"$S/states"
run_driver session-end A
check "session-end, a report NOT engaged: exit 6" "$RC" "6"
has "session-end, a report NOT engaged: said, before anything is removed" "$OUT" "session guard: NOT engaged: the recovery OS reports it NOT engaged: btrbk.timer is not masked (loaded);"
check "session-end, a report NOT engaged: the disk given back all the same" "$(file "$S/attached.xml")" ""
check "session-end, a report NOT engaged: lock free" "$(lock_state)" "free"

for sig in TERM HUP; do
    fixture
    write_state 3 "$(record_json system-recovery-A-2tb may)"
    touch "$S/guard_silent"
    for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
    SIG=$sig GUARD_SECS=300 run_interrupted session A
    check "SIG$sig, not judged: kept (exit 3)" "$RC" "3"
    has "SIG$sig, not judged: said" "$OUT" "THE SESSION GUARD HAS NOT BEEN JUDGED"
done
printf 'shut off\n' >"$S/states"
run_driver session-end A
check "session-end, no report from a domain that ran: exit 6" "$RC" "6"
has "session-end, no report from a domain that ran: said" "$OUT" "session guard: NOT engaged: no report from a recovery OS that ran"

fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
GUARD_SECS=300 run_interrupted session A
{ line "$ENG4"; line "$LIFT4" "$BOOT_A" 2; } >"$STATE/system-recovery-A-2tb.guard" 2>/dev/null || :
run_driver status
check "status, engaged: exit 0" "$RC" "0"
has "status, engaged: said" "$OUT" "  Session guard   in the definition; engaged (1 boot; lifted 1 time)"
printf 'shut off\n' >"$S/states"
run_driver session-end A
check "session-end, engaged: exit 0" "$RC" "0"
has "session-end, engaged: said" "$OUT" "session guard: engaged (1 boot; lifted 1 time)"
check "session-end, engaged: then removed" "$(ls -A "$STATE")" ""

fixture
touch "$S/guard_silent"
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
GUARD_SECS=300 run_interrupted session A
line "das-vm-guard NOT engaged: btrbk.timer is not masked (loaded);" >"$STATE/system-recovery-A-2tb.guard" 2>/dev/null || :
printf 'shut off\n' >"$S/states"
run_driver session-end A
check "session-end, NOT engaged on a no record: exit 5" "$RC" "5"

fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
GUARD_SECS=300 run_interrupted session A
rm -f "$STATE/system-recovery-A-2tb.guard"
mkdir "$STATE/system-recovery-A-2tb.guard" 2>/dev/null || :
printf 'shut off\n' >"$S/states"
run_driver session-end A
check "session-end, a report it cannot read: exit 6" "$RC" "6"
has "session-end, a report it cannot read: said" "$OUT" "cannot be read"

echo "--- a session's guard left behind: session-end takes it out, under the lock"
fixture
run_driver session A
cp "$S/defined.at_start.xml" "$S/defined.xml" 2>/dev/null || :
run_driver status
has "status: a guard left in the definition" "$OUT" "  Session guard   in the definition"
check "status, a guard left without its session state: exit 6" "$RC" "6"
has "status, a guard left without its session state: cannot be judged" "$OUT" "  Session guard   in the definition; cannot be judged: no session state for it"
rm -f "$S/lock.at_define"
run_driver session-end A
check "session-end, guard left, its session state gone: exit 6" "$RC" "6"
has "session-end, guard left, its session state gone: says it cannot be judged" "$OUT" "session guard: cannot be judged"
has "session-end, guard left: says so" "$OUT" "the session guard was still in recovery-os-updater-system-recovery-A-2tb's definition and is out of it now"
lacks "session-end, guard left: out of the definition" "$(file "$S/defined.xml")" "io.systemd.credential"
check "session-end, guard left: from the template" "$(tail -n 2 "$S/virsh.calls" | head -n 1)" "define --validate $STATE/system-recovery-A-2tb.plain.xml"
matches "session-end, guard left: under the maintenance lock, its own" "$(tail -n 1 "$S/lock.at_define")" "^recovery-os VM session system-recovery-A-2tb pid [0-9]+$"
check "session-end, guard left: the lock free again" "$(lock_state)" "free"
run_driver status
has "status: then no guard" "$OUT" "  Session guard   none"
grep -v "org.dasbackup.guard" "$S/defined.at_start.xml" >"$S/defined.xml" || :
run_driver status
has "status: strings left without the port, seen" "$OUT" "  Session guard   in the definition"
grep -v "<entry>" "$S/defined.at_start.xml" >"$S/defined.xml" || :
run_driver status
has "status: the port left without the strings, seen" "$OUT" "  Session guard   in the definition"
cp "$S/defined.at_start.xml" "$S/defined.xml" 2>/dev/null || :
start_blocker
run_driver session-end A
check "session-end, guard left, the lock held: refused" "$RC" "1"
has "session-end, guard left, the lock held: says by whom" "$OUT" "is held by"
has "session-end, guard left, the lock held: the guard stays" "$(file "$S/defined.xml")" "org.dasbackup.guard"
stop_blocker
cp "$S/defined.at_start.xml" "$S/defined.xml" 2>/dev/null || :
touch "$S/define_fail_plain"
run_driver session-end A
check "session-end, guard left, cannot take it out: nonzero" "$((RC != 0))" "1"
has "session-end, guard left, cannot take it out: says so" "$OUT" "the session guard is still in recovery-os-updater-system-recovery-A-2tb's definition"

echo "--- izn9: after the guard confirmed, silence is a warning; after a reset, a new boot must confirm"
# The masks' digest, as a heartbeat names them.
ENG4C="das-vm-guard engaged 4 masks sha256:$(printf '%s' btrbk.service,btrbk.timer,cronie.service,crond.service | sha256sum | cut -c1-16)"
# A guest that runs $1 looks, then powers off by itself -- or when asked.
runs_for() {
    for ((i = 0; i < $1; i++)); do echo running; done >"$S/states.running"
    echo "shut off" >>"$S/states.running"
    printf 'shut off\n' >"$S/states.after_shutdown"
}

# The reporter dies after the guard confirmed: the guard itself (the bind
# mount, the masks) does not depend on it, so nothing is stopped for it -- a
# warning, said again, and in the summary.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
echo "3 silence" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=1 run_driver session A
check "reporter silent after it confirmed, may: a warning, not a stop (exit 5)" "$RC" "5"
lacks "reporter silent after it confirmed, may: never asked to shut down" "$(events)" "virsh shutdown"
check "reporter silent after it confirmed, may: never destroyed" "$(file "$S/forbidden")" ""
matches "reporter silent after it confirmed, may: the warning, said again" "$(grep -c "THE SESSION GUARD'S REPORTER IS SILENT" <<<"$OUT")" "^([2-9]|[1-9][0-9]+)$"
has "reporter silent after it confirmed, may: both causes it cannot tell apart" "$OUT" "Either its reporter stopped -- the guard itself, the bind mount and the masks, does not depend on it -- or the recovery OS has hung: this script cannot tell which, and does not stop it for that"
has "reporter silent after it confirmed, may: how to look" "$OUT" "Look: virt-viewer --connect qemu:///system --attach recovery-os-updater-system-recovery-A-2tb (inside it: systemctl status das-vm-guard-report das-vm-guard)"
has "reporter silent after it confirmed, may: in the summary" "$OUT" "Warnings      the session guard's reporter went silent 1 time"
has "reporter silent after it confirmed, may: the guard engaged all the same" "$OUT" "Guard         engaged -- $ENG4 (1 boot; lifted 0 times)"
check "reporter silent after it confirmed, may: the reset watch ended by the driver" "$(watch_ended)" "1 started, 1 ended by the driver"

# ...unless the reset watch is down: then a reset cannot be ruled out, and
# silence fails closed as a boot without the guard would.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
echo "3 silence" >"$S/guest.script"
touch "$S/event_dies"
DRIVER_TIMEOUT=40 GUARD_SECS=1 run_driver session A
check "reset watch down, then silence, may: shut down (exit 6)" "$RC" "6"
has "reset watch down, then silence, may: the watch's end said, with why" "$OUT" "THE RESET WATCH HAS STOPPED (libvirt's event stream ended: error: internal error: client socket is closed)"
has "reset watch down, then silence, may: said" "$OUT" "with the reset watch down (libvirt's event stream ended"
has "reset watch down, then silence, may: asked to shut down" "$(events)" "virsh shutdown"
check "reset watch down, then silence, may: never destroyed" "$(file "$S/forbidden")" ""

# A reset, then nothing from a new boot: a boot that came without the guard
# says nothing -- shut down (ACPI), exit 6.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
printf '3 reset %s\n3 silence\n' "$BOOT_B" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=1 run_driver session A
check "a reset, then silence, may: shut down (exit 6)" "$RC" "6"
has "a reset, then silence, may: the reset seen" "$OUT" "the recovery OS was reset (a reboot inside it, or virsh reset): a boot after that must report its guard engaged within 0h 00m 01s"
has "a reset, then silence, may: said" "$OUT" "the session guard did not confirm: the recovery OS was reset, and no boot after that reported its guard engaged within 0h 00m 01s"
has "a reset, then silence, may: asked to shut down" "$(events)" "virsh resume|virsh shutdown|virsh detach"
check "a reset, then silence, may: never destroyed" "$(file "$S/forbidden")" ""

# A reset, then the new boot reports engaged: nothing to do.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 12
printf '3 reset %s\n' "$BOOT_B" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=2 run_driver session A
check "a reset, then the new boot engaged, may: exit 0" "$RC" "0"
has "a reset, then the new boot engaged, may: said" "$OUT" "the boot after the reset reports its guard engaged (boot $BOOT_B)"
has "a reset, then the new boot engaged, may: in the summary" "$OUT" "Guard         engaged -- $ENG4 (2 boots; lifted 0 times; 1 reset)"
lacks "a reset, then the new boot engaged, may: no shutdown" "$(events)" "virsh shutdown"

# A reset, then lines only of the boot from before it: no new boot answered.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
printf '3 reset %s\n' "$BOOT_A" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=1 run_driver session A
check "a reset, then only the old boot's lines, may: shut down (exit 6)" "$RC" "6"
has "a reset, then only the old boot's lines, may: said" "$OUT" "no boot after that reported its guard engaged"

# A boot loop: a reset every few looks, no boot reporting. The deadline runs
# from the first reset not answered -- the next ones do not push it out.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 200
for ((p = 3; p < 200; p += 3)); do printf '%s reset %s\n%s silence\n' "$p" "$BOOT_B" "$p"; done >"$S/guest.script"
DRIVER_TIMEOUT=60 GUARD_SECS=2 run_driver session A
check "a boot loop of resets, none reporting, may: exit 6" "$RC" "6"
has "a boot loop of resets, none reporting, may: asked to shut down while it ran" "$(events)" "virsh resume|virsh shutdown|"
has "a boot loop of resets, none reporting, may: said" "$OUT" "no boot after that reported its guard engaged within 0h 00m 02s"

# Off before a boot after a reset reported: the same verdict.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 4
printf '3 reset %s\n3 silence\n' "$BOOT_B" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=300 run_driver session A
check "a reset, then off before a new boot reported, may: exit 6" "$RC" "6"
has "a reset, then off before a new boot reported, may: said" "$OUT" "the recovery OS was reset, and powered off before a boot after that reported its guard engaged"

# A new boot with no reset (kexec): it must begin with its line 1, engaged.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 8
printf '3 boot %s\n' "$BOOT_B" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=300 run_driver session A
check "a new boot without a reset, engaged: exit 0" "$RC" "0"
has "a new boot without a reset, engaged: said" "$OUT" "the recovery OS booted again (boot $BOOT_B), and its guard is engaged (2 boots so far)"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
printf '3 boot %s 2\n' "$BOOT_B" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=300 run_driver session A
check "a new boot without a reset, its line 1 lost, may: exit 6" "$RC" "6"
has "a new boot without a reset, its line 1 lost, may: said" "$OUT" "boot $BOOT_B: its first report was not seen (this is its line 2)"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
printf '3 boot %s\n3 lift\n' "$BOOT_B" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=300 run_driver session A
check "a new boot without a reset, lifted from its start, may: exit 6" "$RC" "6"
has "a new boot without a reset, lifted from its start, may: said" "$OUT" "boot $BOOT_B began without the guard engaged"

# Heartbeats name the masks by their digest; a full line again in the same
# state is a reporter that started again -- said, not a failure.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
{ line "$ENG4"; line "$ENG4C" "$BOOT_A" 2; line "$ENG4" "$BOOT_A" 3; } >"$S/guard_reply"
run_driver session A
check "heartbeats by digest, a reporter that started again: exit 0" "$RC" "0"
has "heartbeats by digest, a reporter that started again: said" "$OUT" "the guard's reporter in the recovery OS started again (boot $BOOT_A); the guard does not depend on it"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
{ line "$ENG4"; line "das-vm-guard engaged 4 masks sha256:0000000000000000" "$BOOT_A" 2; } >"$S/guard_reply"
run_driver session A
check "a heartbeat with another digest, may: exit 6" "$RC" "6"
has "a heartbeat with another digest, may: said" "$OUT" "a report line that is no report: 'das-vm-guard engaged 4 masks sha256:0000000000000000 seq 2"
# Lines of a boot that never arrived: noted, never a failure.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
{ line "$ENG4"; line "$ENG4C" "$BOOT_A" 4; } >"$S/guard_reply"
run_driver session A
check "lines of a boot that never arrived: a warning (exit 5)" "$RC" "5"
has "lines of a boot that never arrived: said" "$OUT" "Warnings      the session guard's report: 2 lines of boot $BOOT_A never arrived (its 2 to 3): a NOT engaged one among them would not have been seen"

echo "--- N5: virtlogd's rotation of the report is followed, never a failure"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 8
echo "3 rotate" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=300 run_driver session A
check "the report rotated: exit 0" "$RC" "0"
lacks "the report rotated: nothing lost" "$OUT" "Warnings"
has "the report rotated: every line judged" "$OUT" "Guard         engaged -- $ENG4 (1 boot; lifted 0 times)"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 8
printf '3 rotate\n3 rotate\n4 rotate-split\n' >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=300 run_driver session A
check "rotated twice between two looks, then a line cut by a rotation: exit 0" "$RC" "0"
lacks "rotated twice between two looks, then a line cut by a rotation: nothing lost, nothing odd" "$OUT" "Warnings"
# A NOT engaged line written just before the rotation, unread: found in .0.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
printf '3 notengaged\n3 rotate\n' >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=300 run_driver session A
check "a NOT engaged line in the unread end of the rotated file, may: exit 6" "$RC" "6"
has "a NOT engaged line in the unread end of the rotated file, may: said" "$OUT" "the recovery OS reports it NOT engaged: btrbk.timer is not masked (loaded);"

echo "--- neither a host suspend nor a paused domain is the guest's silence"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 100
printf '0 silence\n12 talk\n' >"$S/guest.script"
GUARD_SECS=5 CLOCK_GAP=4 STOP_SECS=8 run_stopped session A
# The gap is a warning (exit 5), never a failure: this script cannot tell a
# host suspend from its own stop, and in the second the guest ran unwatched.
check "stopped 8 s while the first report is due within 5 s: a warning, not a failure (exit 5)" "$RC" "5"
matches "stopped 8 s while the first report is due within 5 s: the gap in the summary" "$OUT" "Warnings      0h 00m 0[89]s passed in 1 gap between two looks at the recovery OS, and the guard's deadlines and its silence did not count it"
has "stopped 8 s while the first report is due within 5 s: the gap said" "$OUT" "passed between two looks at the recovery OS (a host suspend, or this script was stopped): the guard's deadlines and its silence do not count that time"
lacks "stopped 8 s while the first report is due within 5 s: no failure" "$OUT" "did not confirm"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
{
    for ((i = 0; i < 3; i++)); do echo running; done
    for ((i = 0; i < 60; i++)); do echo paused; done
    for ((i = 0; i < 3; i++)); do echo running; done
    echo "shut off"
} >"$S/states.running"
DRIVER_TIMEOUT=40 GUARD_SECS=2 run_driver session A
check "paused for longer than the deadline, nothing reported meanwhile: exit 0" "$RC" "0"
lacks "paused for longer than the deadline: no silence counted" "$OUT" "SILENT"
has "paused for longer than the deadline: it was paused" "$OUT" "domain state: running -> paused"

echo "--- N1: a managed-save image refuses the session; the start is always afresh"
fixture
echo yes >"$S/managed_save"
run_driver session A
check "managed save: refused" "$RC" "1"
has "managed save: says what it is" "$OUT" "has a managed-save image: the saved memory of a recovery OS that was RUNNING when it was saved"
has "managed save: never discarded by the script" "$OUT" "This script never discards it: the decision is yours"
lacks "managed save: not discarded" "$(file "$S/virsh.calls")" "managedsave-remove"
check "managed save: nothing locked" "$(file "$S/flock.calls")" ""
lacks "managed save: never started" "$(events)" "virsh start"
for v in unknown none; do
    fixture
    echo "$v" >"$S/managed_save"
    run_driver session A
    check "managed save $v: refused" "$RC" "1"
    has "managed save $v: says it cannot tell" "$OUT" "cannot tell whether recovery-os-updater-system-recovery-A-2tb has a managed-save image"
done
fixture
touch "$S/managed_save_at_attach"
run_driver session A
check "managed save appearing before the start: refused" "$RC" "1"
lacks "managed save appearing before the start: never started" "$(events)" "virsh start"
check "managed save appearing before the start: the disk given back" "$(file "$S/attached.xml")" ""
check "managed save appearing before the start: lock free" "$(lock_state)" "free"
fixture
run_driver session A
has "the start: paused and afresh (--force-boot), never from a saved image" "$(file "$S/virsh.calls")" "start --paused --force-boot recovery-os-updater-system-recovery-A-2tb"

echo "--- the destroy path: what it means, said (the operator's review requirement)"
for how in live_drops_guard start_error_but_started; do
    fixture
    write_state 3 "$(record_json system-recovery-A-2tb may)"
    touch "$S/$how"
    run_driver session A
    check "destroyed ($how): exit 1" "$RC" "1"
    has "destroyed ($how): destroyed while paused" "$(events)" "virsh start|virsh destroy|"
    has "destroyed ($how): nothing ran" "$OUT" "Nothing ran in recovery-os-updater-system-recovery-A-2tb: it was destroyed while still paused, before it ran a single instruction -- no firmware, no OS, no write to the disk."
    has "destroyed ($how): a retry is safe" "$OUT" "A retry is safe: run the session again."
    has "destroyed ($how): a repeat means a persistent cause to fix" "$OUT" "If it fails the same way again, the cause is persistent -- libvirt not applying the session guard's SMBIOS strings to the started domain, for one -- and needs a fix before any session can boot"
    lacks "destroyed ($how): the guard out of the definition" "$(file "$S/defined.xml")" "oemStrings"
    check "destroyed ($how): the template itself defined again" "$(file "$S/defined.xml")" "$(rendered A)"
    check "destroyed ($how): lock free" "$(lock_state)" "free"
    check "destroyed ($how): the reset watch ended by the driver" "$(watch_ended)" "1 started, 1 ended by the driver"
done

echo "--- N2: destroy reads the state itself, right before"
# Something else resumes the domain while the driver reads it: never
# destroyed; an OS without the guard, it is asked to shut down.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/live_drops_guard" "$S/resume_at_live_dumpxml"
runs_for 60
DRIVER_TIMEOUT=40 run_driver session A
check "resumed by another at the check, may: exit 6" "$RC" "6"
check "resumed by another at the check, may: never destroyed" "$(file "$S/forbidden")" ""
lacks "resumed by another at the check, may: no destroy" "$(file "$S/virsh.calls")" "destroy"
has "resumed by another at the check, may: said" "$OUT" "not destroying recovery-os-updater-system-recovery-A-2tb: it is running, not paused"
has "resumed by another at the check, may: asked to shut down" "$(events)" "virsh start|virsh shutdown|virsh detach"
# Interrupted while paused; by the time on_exit destroys, something else has
# resumed it: never destroyed, kept.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
printf 'paused\nrunning\n' >"$S/states.after_start"
touch "$S/dumpxml_live_sleep"
WAIT_FILE="$S/in_live_dumpxml" run_interrupted session A
check "interrupted while paused, resumed by another before the destroy: kept (exit 3)" "$RC" "3"
check "interrupted while paused, resumed by another before the destroy: never destroyed" "$(file "$S/forbidden")" ""
has "interrupted while paused, resumed by another before the destroy: said" "$OUT" "not destroying recovery-os-updater-system-recovery-A-2tb: it is running, not paused"
# Interrupted while paused, still paused: destroyed, and what that means said.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/dumpxml_live_sleep"
WAIT_FILE="$S/in_live_dumpxml" run_interrupted session A
check "interrupted while paused: exit 1" "$RC" "1"
has "interrupted while paused: destroyed" "$(events)" "virsh start|virsh destroy|"
has "interrupted while paused: a retry is safe" "$OUT" "A retry is safe: run the session again."

echo "--- N3: past the bound, the facts and the trade-off; destroy only as the operator's choice"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
for ((i = 0; i < 400; i++)); do echo running; done >"$S/states.running"
DRIVER_TIMEOUT=30 GRACE=3 RESEND=1 GUARD_SECS=1 run_driver session A
check "never goes, may: kept (exit 3)" "$RC" "3"
matches "never goes, may: the facts -- asked how often, over how long" "$OUT" "It was asked to shut down [2-9] times over 0h 00m 0[2-9]s \\(ACPI\\), and has not gone"
has "never goes, may: the operator's choice, which the script never makes" "$OUT" "Forcing it off is your choice to make, and this script never"
has "never goes, may: one side of the trade-off" "$OUT" "on one side, a recovery OS without a confirmed session guard, running beside the backups on $DISK_A"
has "never goes, may: the other side" "$OUT" "on the other, an update in it cut short"
lacks "never goes, may: no clock on it" "$OUT" "within a minute"
lacks "never goes, may: not called an update for a guard stop" "$OUT" "it may still be updating"
check "never goes, may: the reset watch ended by the driver" "$(watch_ended)" "1 started, 1 ended by the driver"
fixture
printf 'running\n' >"$S/states.running"
run_driver session A --timeout 1
has "--timeout, never goes: it may still be updating" "$OUT" "-- it may still be updating"
check "--timeout, never goes: the reset watch ended by the driver" "$(watch_ended)" "1 started, 1 ended by the driver"
# Not judged: status first, before any power-off.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent"
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
GUARD_SECS=300 run_interrupted session A
check "not judged: kept (exit 3)" "$RC" "3"
matches "not judged: status first, then the power-off" "$(tr '\n' ' ' <<<"$OUT")" "0\\. Judge the guard first: .*recovery-os-vm\\.sh status .* 1\\. Power it off now"
# A resume that failed leaves it paused: never told to power it off from a
# console that runs nothing.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/resume_fail"
run_driver session A
check "paused after a failed resume: kept (exit 3)" "$RC" "3"
has "paused after a failed resume: resume it first" "$OUT" "virsh --connect qemu:///system resume recovery-os-updater-system-recovery-A-2tb"
lacks "paused after a failed resume: never systemctl poweroff in a paused guest" "$OUT" "systemctl poweroff"
has "paused after a failed resume: or destroy, as the operator's choice" "$OUT" "on one side, a paused recovery OS that may have run"

echo "--- lorz: the reset watch's virsh is waited for, and killed only past its time"
# Whether the reset watch's virsh (the first the stub recorded) still runs.
event_virsh_state() {
    local pid st
    pid=$(head -n 1 "$S/event.pids") || pid=""
    [[ -n "$pid" ]] || { echo "never started"; return; }
    [[ -d "/proc/$pid" ]] || { echo gone; return; }
    st=$(awk '/^State:/ {print $2}' "/proc/$pid/status" 2>/dev/null) || st=""
    case "$st" in
    Z) echo gone ;;
    "") if [[ -d "/proc/$pid" ]]; then echo "unreadable"; else echo gone; fi ;;
    *) echo "still runs ($st)" ;;
    esac
}
# How many of this fixture's stub virsh event processes run (not zombies):
# found by their command line, which names this fixture's own stub, so one
# that never got as far as writing its pid down is counted too.
virsh_events_left() {
    local p cmd st n=0 args=()
    for p in /proc/[0-9]*; do
        { mapfile -d '' -t args <"$p/cmdline"; } 2>/dev/null || continue
        cmd="${args[*]} "
        [[ "$cmd" == *"$T/bin/virsh "*" event "* ]] || continue
        { read -r st <"$p/stat"; } 2>/dev/null || continue
        st=${st##*) }
        [[ "${st%% *}" == Z ]] || n=$((n + 1))
    done
    echo "$n"
}
# Slow to end on the SIGTERM it was sent, as on a loaded host: waited for,
# so it ends on that signal -- killed at once, it could say nothing, and the
# driver went on before it had gone.
fixture
touch "$S/event_slow_end"
run_driver session A
check "a virsh slow to end: exit 0" "$RC" "0"
check "a virsh slow to end: the reset watch ended by the driver" "$(watch_ended)" "1 started, 1 ended by the driver"
check "a virsh slow to end: gone when the driver ended" "$(event_virsh_state)" "gone"
# The same where the session is kept (the guard and its files stay).
fixture
touch "$S/event_slow_end"
printf 'running\n' >"$S/states.running"
run_driver session A --timeout 1
check "a virsh slow to end, session kept: exit 3" "$RC" "3"
check "a virsh slow to end, session kept: the reset watch ended by the driver" "$(watch_ended)" "1 started, 1 ended by the driver"
check "a virsh slow to end, session kept: gone when the driver ended" "$(event_virsh_state)" "gone"
# Deaf to SIGTERM: killed past its time, and gone before the driver ends.
fixture
touch "$S/event_ignores_term"
run_driver session A
check "a virsh deaf to SIGTERM: exit 0" "$RC" "0"
check "a virsh deaf to SIGTERM: killed before the driver ended" "$(event_virsh_state)" "gone"
check "a virsh deaf to SIGTERM: killed, it said nothing" "$(watch_ended)" "1 started, 0 ended by the driver"
# The watch itself cannot end on its SIGTERM (stopped, and never let go):
# killed past its time, and the virsh -- which it never told -- ended by the
# driver's own SIGTERM, on which it says it ended.
fixture
for ((i = 0; i < 60; i++)); do echo running; done >"$S/states.running"
echo "shut off" >>"$S/states.running"
run_watch_hung session A
check "a watch that cannot end: found, and stopped" "${WATCH_PID:+found}" "found"
check "a watch that cannot end: exit 0" "$RC" "0"
check "a watch that cannot end: killed before the driver ended" "$(if [[ -n "$WATCH_PID" && -d "/proc/$WATCH_PID" ]]; then echo "still there"; else echo gone; fi)" "gone"
check "a watch that cannot end: its virsh ended by the driver" "$(watch_ended)" "1 started, 1 ended by the driver"
check "a watch that cannot end: its virsh gone when the driver ended" "$(event_virsh_state)" "gone"
# A SIGTERM to the driver the moment its virsh exists -- the start of the
# watch is no window in which that virsh can be lost: ended by the driver,
# and gone when it ends.
fixture
touch "$S/event_terms_driver"
run_driver session A
check "a SIGTERM as the virsh starts: the virsh ended by the driver" "$(watch_ended)" "1 started, 1 ended by the driver"
check "a SIGTERM as the virsh starts: gone when the driver ended" "$(event_virsh_state)" "gone"
check "a SIGTERM as the virsh starts: nothing forbidden" "$(file "$S/forbidden")" ""
# No stub can reach the one window left -- the virsh started, its pid not yet
# taken from $! -- so this test's copy of the driver signals itself there:
# the trap runs between those two commands, and on_exit must still find the
# virsh, end it, and leave nothing running.
fixture
sed -i '/^    RESET_VIRSH_PID=\$!$/i\    kill -TERM $$' "$DRIVER"
check "a SIGTERM before the virsh's pid is taken: injected, once" "$(grep -c '^    kill -TERM \$\$$' "$DRIVER")" "1"
run_driver session A
# Ended so soon, it may not have become virsh yet, nor said it started: what
# counts is that none of this fixture's runs on.
check "a SIGTERM before the virsh's pid is taken: no virsh event left running" "$(virsh_events_left)" "0"
check "a SIGTERM before the virsh's pid is taken: nothing forbidden" "$(file "$S/forbidden")" ""
# The same while the virsh is still forked but not yet virsh (held there a
# second, in this copy only): its command line is still the driver's, so it
# is known to run by being the driver's child, not by its name -- or it would
# be waited for, and become a virsh no one ends.
fixture
# shellcheck disable=SC2016 # lines of the driver, matched and inserted as written
sed -i -e '/^    RESET_VIRSH_PID=\$!$/i\    kill -TERM $$' \
    -e '/^        exec virsh --connect "\$LIBVIRT_URI" event /i\        sleep 1' "$DRIVER"
check "a SIGTERM before the virsh has become virsh: injected, once each" \
    "$(grep -c '^    kill -TERM \$\$$' "$DRIVER") $(grep -c '^        sleep 1$' "$DRIVER")" "1 1"
DRIVER_TIMEOUT=20 run_driver session A
check "a SIGTERM before the virsh has become virsh: the driver ended (not timed out)" "$([[ "$RC" != 124 ]] && echo ended)" "ended"
sleep 1.5
check "a SIGTERM before the virsh has become virsh: no virsh event left running" "$(virsh_events_left)" "0"

echo "--- N4: status counts silence (it cannot see resets); information only"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
GUARD_SECS=300 run_interrupted session A
check "N4: the session is kept, engaged (exit 3)" "$RC" "3"
calls_before=$(grep -c . "$S/virsh.calls")
GUARD_SECS=300 run_driver status
check "status, a report written just now: exit 0" "$RC" "0"
has "status, a report written just now: its age" "$OUT" "engaged (1 boot; lifted 0 times); its last report 0h 00m 0"
touch "$S/guest.silent"   # the guest's reporter stops
touch -d '-20 minutes' "$STATE/system-recovery-A-2tb.guard"
GUARD_SECS=300 run_driver status
check "status, a report silent past the deadline, may: exit 6" "$RC" "6"
has "status, a report silent past the deadline: said" "$OUT" "NOT confirmed: silent for 0h 20m"
has "status, a report silent past the deadline: both causes" "$OUT" "the reporter stopped, or a boot came without the guard (status cannot see a reset made after the session's driver ended"
printf 'paused\n' >"$S/states"
GUARD_SECS=300 run_driver status
check "status, silent but paused: not counted (exit 0)" "$RC" "0"
lacks "status never stops anything" "$(tail -n +"$((calls_before + 1))" "$S/virsh.calls")" "shutdown"
lacks "status never destroys anything" "$(tail -n +"$((calls_before + 1))" "$S/virsh.calls")" "destroy"
fixture
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
GUARD_SECS=300 run_interrupted session A
touch "$S/guest.silent"   # the guest's reporter stops
touch -d '-20 minutes' "$STATE/system-recovery-A-2tb.guard"
GUARD_SECS=300 run_driver status
check "status, a report silent past the deadline, no record: exit 5" "$RC" "5"

echo "--- the last look: what reached the report just before an interrupt is judged"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
# shellcheck disable=SC2016 # expanded by run_interrupted's eval, at the signal
POLL=3 BEFORE_SIG='line "das-vm-guard NOT engaged: btrbk.timer is not masked (loaded);" "$BOOT_A" 2 >>"$STATE/system-recovery-A-2tb.guard"' run_interrupted session A
check "a NOT engaged line just before the interrupt: kept (exit 3)" "$RC" "3"
has "a NOT engaged line just before the interrupt: said" "$OUT" "THE RECOVERY OS IS RUNNING WITHOUT A CONFIRMED SESSION GUARD (the recovery OS reports it NOT engaged: btrbk.timer is not masked (loaded);"

echo "--- smaller ones"
fixture
touch "$S/guard_silent"
printf 'shut off\n' >"$S/states.running"
POLL=2 GUARD_SECS=300 run_interrupted session A
check "interrupted as it powers off, no report, no record: exit 5" "$RC" "5"
fixture
rm "$S/defined"
run_driver status
check "status, the domain not defined: exit 0" "$RC" "0"
has "status, the domain not defined: no guard" "$OUT" "  Session guard   none (recovery-os-updater-system-recovery-A-2tb is not defined:"
touch "$S/list_fail"
run_driver status
check "status, the domain cannot be read: exit 6" "$RC" "6"
has "status, the domain cannot be read: unknown" "$OUT" "  Session guard   unknown"

echo "--- round 4: a reset the watch read late is still a reset (I-1)"
# The driver and its reset watch stopped -- as a Ctrl-Z to the session's
# terminal stops them; not the stub's virsh event, whose stopped bash would
# hang in its wait (a stub artifact) -- once the driver waits for the guest.
# libvirt's reboot event is emitted into the stopped watch's pipe, then
# DURING_STOP runs. The watch is let go first and given 3 s to record the
# reset (WATCH_FIRST, the default), then the driver -- or both at once
# (WATCH_FIRST=no). The driver then runs to its end.
run_watch_stopped() {
    local i dpid epid wpid
    RC=0
    set -m
    driver_env bash "$DRIVER" "$@" >"$T/driver.out" 2>&1 &
    dpid=$!
    set +m
    for ((i = 0; i < 200; i++)); do
        grep -q 'waiting for the recovery OS to power off' "$T/driver.out" && break
        sleep 0.05
    done
    sleep 0.3
    epid=$(head -n 1 "$S/event.pids")
    if ! wpid=$(reset_watch_of "$epid" "$dpid"); then
        echo "(the reset watch was not found: no child of the driver $dpid reads virsh event $epid)" >>"$T/driver.out"
        wpid=$dpid
    fi
    kill -STOP "$dpid" "$wpid"
    echo reset >>"$S/reboots.pending"
    sleep 0.3
    if [[ -n "${DURING_STOP:-}" ]]; then eval "$DURING_STOP"; fi
    if [[ "${WATCH_FIRST:-yes}" == yes ]]; then
        kill -CONT "$wpid"
        for ((i = 0; i < 60; i++)); do
            grep -q '^reset ' "$STATE"/*.resets 2>/dev/null && break
            sleep 0.05
        done
    fi
    kill -CONT "$wpid" "$dpid"
    for ((i = 0; i < 1200; i++)); do
        kill -0 "$dpid" 2>/dev/null || break
        sleep 0.05
    done
    if kill -0 "$dpid" 2>/dev/null; then
        kill -KILL "$dpid" 2>/dev/null || :
        echo "(the driver did not end within 60s)" >>"$T/driver.out"
    fi
    wait "$dpid" || RC=$?
    OUT="$(cat "$T/driver.out")"
}
# The guest's next boot (boot B) from the next look on: its line 1, unless silent.
# shellcheck disable=SC2016 # expanded by run_watch_stopped's eval
NEXT_BOOT_B='echo "$BOOT_B" >"$S/guest.boot"; echo 0 >"$S/guest.seq"; echo engaged >"$S/guest.mode"; : >"$S/guest.last"'

fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 100
DURING_STOP=$NEXT_BOOT_B GUARD_SECS=2 run_watch_stopped session A
check "watch stopped across a reset, then the new boot engaged: exit 0" "$RC" "0"
check "watch stopped across a reset, then the new boot engaged: the reset recorded whole" "$(grep -c '^reset ' "$S/resets.at_detach")" "1"
lacks "watch stopped across a reset, then the new boot engaged: no half of it filed apart" "$(file "$S/resets.at_detach")" "said"
has "watch stopped across a reset, then the new boot engaged: proof asked" "$OUT" "the recovery OS was reset (a reboot inside it, or virsh reset): a boot after that must report its guard engaged"
has "watch stopped across a reset, then the new boot engaged: proof given" "$OUT" "the boot after the reset reports its guard engaged (boot $BOOT_B)"
has "watch stopped across a reset, then the new boot engaged: in the summary" "$OUT" "Guard         engaged -- $ENG4 (2 boots; lifted 0 times; 1 reset)"

fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 100
DURING_STOP="$NEXT_BOOT_B; touch \"\$S/guest.silent\"" GUARD_SECS=2 run_watch_stopped session A
check "watch stopped across a reset, then silence, may: shut down (exit 6)" "$RC" "6"
has "watch stopped across a reset, then silence, may: said" "$OUT" "the session guard did not confirm: the recovery OS was reset, and no boot after that reported its guard engaged within 0h 00m 02s"
has "watch stopped across a reset, then silence, may: asked to shut down" "$(events)" "virsh shutdown"
check "watch stopped across a reset, then silence, may: never destroyed" "$(file "$S/forbidden")" ""

# The new boot's line 1 written while the watch was stopped: the reset is
# recorded where the report stood when the watch read it -- after that line
# -- so the line cannot answer it, and a proof is still asked for. The
# cautious side: a boot is never let off for want of knowing when the reset
# was.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 100
# shellcheck disable=SC2016 # expanded by run_watch_stopped's eval
DURING_STOP="$NEXT_BOOT_B"'; line "$ENG4" "$BOOT_B" 1 >>"$(cat "$S/guard_port")"; echo 1 >"$S/guest.seq"; echo engaged >"$S/guest.last"' \
    WATCH_FIRST=no GUARD_SECS=2 run_watch_stopped session A
check "watch stopped, the new boot's line 1 before the watch read the reset, may: proof still asked (exit 6)" "$RC" "6"
has "watch stopped, the new boot's line 1 before the watch read the reset: the reset recorded" "$OUT" "the recovery OS was reset (a reboot inside it, or virsh reset)"
has "watch stopped, the new boot's line 1 before the watch read the reset: said" "$OUT" "no boot after that reported its guard engaged within 0h 00m 02s"

# Output of the event stream not recognised, that names a reboot: a reset.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
touch "$S/event_garbled"
printf '3 reset %s\n3 silence\n' "$BOOT_B" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=1 run_driver session A
check "event output not recognised, naming a reboot, then silence, may: a reset (exit 6)" "$RC" "6"
has "event output not recognised, naming a reboot: kept, said" "$(file "$S/resets.at_detach")" "said event 'reboot' for domain: 'recovery-os-updater-system-recovery-A-2tb'"
has "event output not recognised, naming a reboot: proof asked" "$OUT" "the session guard did not confirm: the recovery OS was reset, and no boot after that reported its guard engaged"

echo "--- round 4: a reset the session saw is honoured at its end, by status and by session-end (I-2)"
for sig in INT HUP; do
    fixture
    write_state 3 "$(record_json system-recovery-A-2tb may)"
    for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
    printf '3 reset %s\n3 silence\n' "$BOOT_B" >"$S/guest.script"
    # shellcheck disable=SC2016 # expanded by run_interrupted's eval
    SIG=$sig BEFORE_SIG='for ((k = 0; k < 200; k++)); do grep -q "was reset" "$T/driver.out" && break; sleep 0.05; done' GUARD_SECS=300 run_interrupted session A
    check "SIG$sig after a reset, the boot after it silent: kept (exit 3)" "$RC" "3"
    has "SIG$sig after a reset, the boot after it silent: not judged, said" "$OUT" "THE BOOT AFTER THE RESET HAS NOT BEEN JUDGED: the recovery OS was reset, and no boot after that has reported its guard engaged yet"
    lacks "SIG$sig after a reset, the boot after it silent: never called engaged" "$OUT" "the session guard is engaged (das-vm-guard"
    lacks "SIG$sig after a reset, the boot after it silent: never told to let the update finish" "$OUT" "Let the update finish, then power the recovery OS off from inside it."
    has "SIG$sig after a reset, the boot after it silent: status first" "$OUT" "0. Judge the guard first:"
    GUARD_SECS=300 run_driver status
    check "SIG$sig, status within the deadline after the reset: pending (exit 0)" "$RC" "0"
    has "SIG$sig, status within the deadline after the reset: said" "$OUT" "  Session guard   in the definition; pending: the recovery OS was reset 0h 00m"
    lacks "SIG$sig, status within the deadline after the reset: never engaged" "$OUT" "in the definition; engaged"
    # The reset 20 minutes ago (as the watch recorded it): past the deadline.
    sed -i -E "s/^(reset [0-9-]+ [0-9]+) [0-9]+$/\\1 $(($(date +%s) - 1200))/" "$STATE/system-recovery-A-2tb.resets"
    GUARD_SECS=300 run_driver status
    check "SIG$sig, status past the deadline after the reset, may: exit 6" "$RC" "6"
    has "SIG$sig, status past the deadline after the reset: said" "$OUT" "NOT confirmed: the recovery OS was reset 0h 20m"
    printf 'paused\n' >"$S/states"
    GUARD_SECS=300 run_driver status
    check "SIG$sig, status after the reset, paused: pending (exit 0)" "$RC" "0"
    has "SIG$sig, status after the reset, paused: time paused does not count" "$OUT" "it is paused, and time paused does not count"
    printf 'shut off\n' >"$S/states"
    GUARD_SECS=300 run_driver session-end A
    check "SIG$sig, session-end after a reset no boot answered, may: exit 6" "$RC" "6"
    has "SIG$sig, session-end after a reset no boot answered: said" "$OUT" "session guard: NOT confirmed: the recovery OS was reset, and was shut off before a boot after that reported its guard engaged"
    check "SIG$sig, session-end after a reset no boot answered: the resets taken away with the guard" "$(ls "$STATE"/*.resets 2>/dev/null)" ""
done
# The same on a "no" record: 5.
fixture
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
printf '3 reset %s\n3 silence\n' "$BOOT_B" >"$S/guest.script"
# shellcheck disable=SC2016 # expanded by run_interrupted's eval
BEFORE_SIG='for ((k = 0; k < 200; k++)); do grep -q "was reset" "$T/driver.out" && break; sleep 0.05; done' GUARD_SECS=300 run_interrupted session A
printf 'shut off\n' >"$S/states"
GUARD_SECS=300 run_driver session-end A
check "session-end after a reset no boot answered, no record: exit 5" "$RC" "5"
# Answered: a boot after the reset reported engaged -- engaged, exit 0.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
printf '3 reset %s\n' "$BOOT_B" >"$S/guest.script"
# shellcheck disable=SC2016 # expanded by run_interrupted's eval
BEFORE_SIG='for ((k = 0; k < 200; k++)); do grep -q "the boot after the reset reports" "$T/driver.out" && break; sleep 0.05; done' GUARD_SECS=300 run_interrupted session A
check "interrupted after a reset the new boot answered: kept (exit 3)" "$RC" "3"
has "interrupted after a reset the new boot answered: engaged" "$OUT" "the session guard is engaged (das-vm-guard"
GUARD_SECS=300 run_driver status
check "status after a reset the new boot answered: exit 0" "$RC" "0"
has "status after a reset the new boot answered: said" "$OUT" "engaged (2 boots; lifted 0 times); each of its 1 reset answered by a boot reporting engaged"
printf 'shut off\n' >"$S/states"
GUARD_SECS=300 run_driver session-end A
check "session-end after a reset the new boot answered: exit 0" "$RC" "0"

# The deadline after a reset passed by the last look, at an interrupt: NOT
# confirmed, as a look while it ran would have found -- said, never acted on.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
printf '1 reset %s\n1 silence\n' "$BOOT_B" >"$S/guest.script"
# shellcheck disable=SC2016 # expanded by run_interrupted's eval
POLL=4 BEFORE_SIG='for ((k = 0; k < 200; k++)); do grep -q "was reset" "$T/driver.out" && break; sleep 0.05; done; sleep 2' GUARD_SECS=1 run_interrupted session A
check "interrupted after the deadline of a reset passed: kept (exit 3)" "$RC" "3"
has "interrupted after the deadline of a reset passed: NOT confirmed, said" "$OUT" "THE RECOVERY OS IS RUNNING WITHOUT A CONFIRMED SESSION GUARD (the recovery OS was reset, and no boot after that reported its guard engaged within 0h 00m 01s"
lacks "interrupted after the deadline of a reset passed: never asked to shut down by the last look" "$(events)" "virsh shutdown"
# A line of the watch's record that cannot be parsed is still a reset.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
printf '3 badreset\n3 silence\n' >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=1 run_driver session A
check "a reset line that cannot be parsed, then silence, may: a reset (exit 6)" "$RC" "6"
has "a reset line that cannot be parsed: said" "$OUT" "no boot after that reported its guard engaged within 0h 00m 01s"

echo "--- round 4: a line left unfinished at power-off (M-1)"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
{ line "$ENG4"; printf 'das-vm-guard NOT engaged: btrbk.timer is not masked (loaded); seq 2 boot %s' "$BOOT_A"; } >"$S/guard_reply"
runs_for 3
GUARD_SECS=300 run_driver session A
check "off in the middle of a NOT engaged line, may: exit 6" "$RC" "6"
has "off in the middle of a NOT engaged line: said" "$OUT" "the recovery OS powered off in the middle of a line that reports it NOT engaged: 'das-vm-guard NOT engaged: btrbk.timer is not masked (loaded); seq 2 boot"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
{ line "$ENG4"; printf 'das-vm-guard N'; } >"$S/guard_reply"
runs_for 3
GUARD_SECS=300 run_driver session A
check "off in the middle of a line only a NOT engaged one begins so, may: exit 6" "$RC" "6"
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
{ line "$ENG4"; printf 'das-vm-guard engaged 4 ma'; } >"$S/guard_reply"
runs_for 3
GUARD_SECS=300 run_driver session A
check "off in the middle of another line: a warning (exit 5)" "$RC" "5"
has "off in the middle of another line: in the summary" "$OUT" "Warnings      the session guard's report: the recovery OS powered off in the middle of a line, never finished: 'das-vm-guard engaged 4 ma'"
# session-end judges a saved report the same way.
for part in "das-vm-guard NOT engaged: btrbk.timer is not mas" "das-vm-guard engaged 4 ma"; do
    fixture
    write_state 3 "$(record_json system-recovery-A-2tb may)"
    for ((i = 0; i < 4000; i++)); do echo running; done >"$S/states.running"
    GUARD_SECS=300 run_interrupted session A
    touch "$S/guest.silent"
    printf '%s' "$part" >>"$STATE/system-recovery-A-2tb.guard"
    printf 'shut off\n' >"$S/states"
    GUARD_SECS=300 run_driver session-end A
    if [[ "$part" == *NOT* ]]; then
        check "session-end, shut off in the middle of a NOT engaged line, may: exit 6" "$RC" "6"
        has "session-end, shut off in the middle of a NOT engaged line: said" "$OUT" "NOT engaged: the recovery OS was shut off in the middle of a line that reports it NOT engaged"
    else
        check "session-end, shut off in the middle of another line: exit 5" "$RC" "5"
        has "session-end, shut off in the middle of another line: said" "$OUT" "it was shut off in the middle of a line, never finished: 'das-vm-guard engaged 4 ma'"
    fi
done

echo "--- round 4: the destroy reads the vCPUs' time with the state (M-2, ir5c)"
# Resumed and paused again by something else before the check: paused, but
# its vCPUs ran -- never destroyed.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/live_drops_guard" "$S/vcpu_ran"
run_driver session A
check "paused, its vCPUs ran (resumed and paused by another), may: kept (exit 3)" "$RC" "3"
check "paused, its vCPUs ran: never destroyed" "$(file "$S/forbidden")" ""
lacks "paused, its vCPUs ran: no destroy" "$(file "$S/virsh.calls")" "destroy"
has "paused, its vCPUs ran: said" "$OUT" "not destroying recovery-os-updater-system-recovery-A-2tb: it is paused, but its vCPUs have run (1 of 4 with CPU time)"
lacks "paused, its vCPUs ran: never said it ran nothing" "$OUT" "it has not run a single instruction"
has "paused, its vCPUs ran: may have run" "$OUT" "It is paused, and its vCPUs have run: something other than this script resumed it: it may have run"
has "paused, its vCPUs ran: the read made once, with the state" "$(file "$S/virsh.calls")" "domstats --state --vcpu recovery-os-updater-system-recovery-A-2tb"
# Interrupted while paused, its vCPUs ran meanwhile: never destroyed.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/dumpxml_live_sleep" "$S/vcpu_ran"
WAIT_FILE="$S/in_live_dumpxml" run_interrupted session A
check "interrupted while paused, its vCPUs ran: kept (exit 3)" "$RC" "3"
check "interrupted while paused, its vCPUs ran: never destroyed" "$(file "$S/forbidden")" ""
# The read fails, or gives no vCPU time: nothing is destroyed.
for how in domstats_fail domstats_no_vcpu; do
    fixture
    write_state 3 "$(record_json system-recovery-A-2tb may)"
    touch "$S/live_drops_guard" "$S/$how"
    run_driver session A
    check "guard missing, $how: kept (exit 3)" "$RC" "3"
    lacks "guard missing, $how: no destroy" "$(events)" "virsh destroy"
    has "guard missing, $how: said" "$OUT" "not destroying recovery-os-updater-system-recovery-A-2tb: whether it ever ran cannot be read"
    lacks "guard missing, $how: never resumed" "$(events)" "virsh resume"
done

echo "--- round 4: what three mutants of round 3 showed untested (M-3)"
# The domain's state cannot be read once it runs: the guard's clock counts on
# (it fails closed), never stopped as if paused.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
touch "$S/guard_silent" "$S/domstate_fail_after_resume"
DRIVER_TIMEOUT=20 GUARD_SECS=1 run_driver session A
check "state unreadable after the resume, no report, may: kept (exit 3)" "$RC" "3"
has "state unreadable after the resume, no report: the clock counted on" "$OUT" "the session guard did not confirm: no report from the recovery OS within 0h 00m 01s of its start"
has "state unreadable after the resume, no report: asked to shut down" "$(events)" "virsh shutdown"
check "state unreadable after the resume, no report: never destroyed" "$(file "$S/forbidden")" ""
# The file being read is gone (virtlogd past max_backups): the oldest file
# not read yet is read first -- a NOT engaged line there is seen.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
printf '3 rotate\n3 notengaged\n3 rotate\n3 drop\n' >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=300 run_driver session A
check "the file being read gone, a NOT engaged line in the oldest unread, may: exit 6" "$RC" "6"
has "the file being read gone: the loss said" "$OUT" "was gone before it could be read"
has "the file being read gone: the NOT engaged line seen" "$OUT" "the recovery OS reports it NOT engaged: btrbk.timer is not masked (loaded);"
# Two resets in one look, a new boot's line 1 between them: each reset is
# placed where it was, so the first is answered and the second is not.
fixture
write_state 3 "$(record_json system-recovery-A-2tb may)"
runs_for 60
printf '3 reset %s\n3 say\n3 reset %s\n3 silence\n' "$BOOT_B" "$BOOT_A" >"$S/guest.script"
DRIVER_TIMEOUT=40 GUARD_SECS=1 run_driver session A
check "two resets in one look, a boot between them, then silence, may: exit 6" "$RC" "6"
check "two resets in one look: both recorded" "$(grep -c '^reset ' "$S/resets.at_detach")" "2"
has "two resets in one look: the first answered" "$OUT" "the boot after the reset reports its guard engaged (boot $BOOT_B)"
has "two resets in one look: the second not" "$OUT" "no boot after that reported its guard engaged within 0h 00m 01s"

echo "--- the guard's scripts, run as the recovery OS runs them (stand-ins for mount, systemctl, findmnt, sleep)"
fixture
run_driver session A
G="$T/guest"
mkdir -p "$G/bin" "$G/usr/bin" "$G/usr/local/bin" "$G/usr/local/sbin" "$G/creds" "$G/run/das-vm-guard" \
    "$G/dev/virtio-ports" "$G/proc/sys/kernel/random"
# Where the recovery OS's paths land here: each under $G, as guestify puts them.
PORT="$G/dev/virtio-ports/org.dasbackup.guard"
KMSG="$G/dev/kmsg"
# The recovery OS's paths, here.
guestify() {
    local s=${1//\$\$/\$}
    printf '%s' "$s" | sed -E "s#(^|[ \"'=])(/dev/virtio-ports/org\.dasbackup\.guard|/proc/sys/kernel/random/boot_id|/run/das-vm-guard/report\.state|/run/das-vm-guard/btrbk|/run/das-vm-guard|/dev/kmsg|/usr/local/sbin/btrbk|/usr/local/bin/btrbk|/usr/sbin/btrbk|/usr/bin/btrbk|/usr/bin/mount|/sbin/btrbk|/bin/btrbk)#\1$G\2#g"
}
g="$(credential "$S/defined.at_start.xml" systemd.extra-unit.das-vm-guard.service)"
r="$(credential "$S/defined.at_start.xml" systemd.extra-unit.das-vm-guard-report.service)"
credential "$S/defined.at_start.xml" das-vm-guard.btrbk >"$G/creds/das-vm-guard.btrbk"
# One check for all of them: the suite's count must not depend on the driver.
n=0
notshell=""
while IFS= read -r l; do
    gs=$(sed -n "s|^ExecSt[a-z]*=-\{0,1\}\(/usr/bin/timeout [0-9]* \)\{0,1\}/usr/bin/sh -c '\(.*\)'$|\2|p" <<<"$l")
    [[ -n "$gs" ]] || continue
    n=$((n + 1))
    sh -n -c "${gs//\$\$/\$}" 2>/dev/null || notshell+="line $n: ${gs:0:60}; "
done <<<"$g"
check "guard unit: every line of shell is shell" "${notshell:-none}" "none"
check "guard unit: six lines of shell" "$n" "6"

install_line="$(guestify "$(sed -n "s|^ExecStart=/usr/bin/sh -c '\(install .*\)'$|\1|p" <<<"$g")")"
bind="$(guestify "$(sed -n "s|^ExecStart=/usr/bin/sh -c '\(for p in .*\)'$|\1|p" <<<"$g")")"
cat >"$G/usr/bin/mount" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"$GUEST/mount.calls"
rc=$(cat "$GUEST/mount.rc" 2>/dev/null || echo 0)
# A bind that works makes the path the source's own file, as a real one does:
# a hard link here -- the same inode, and the path still the path.
if ((rc == 0)); then ln -f "$2" "${!#}.bound" && mv -f "${!#}.bound" "${!#}"; fi
exit "$rc"
STUB
chmod +x "$G/usr/bin/mount"
# Never the real mount: the script runs only once it names none but the stand-in.
guest_bind() {
    if [[ -z "$bind" || "$bind" != *"$G/usr/bin/mount"* || "$bind" == *" /usr/bin/mount"* ]]; then
        echo "NOT RUN: the script names the real mount"
        return
    fi
    rm -f "$G/mount.calls"
    env GUEST="$G" CREDENTIALS_DIRECTORY="$G/creds" sh -c "$install_line" >/dev/null 2>&1
    env GUEST="$G" sh -c "$bind" >/dev/null 2>&1
    echo "rc=$? $(tr '\n' '|' 2>/dev/null <"$G/mount.calls")"
}
: >"$G/usr/bin/btrbk"
check "guard unit, here: binds the refusing btrbk over the btrbk present" "$(guest_bind)" "rc=0 --bind $G/run/das-vm-guard/btrbk $G/usr/bin/btrbk|"
check "guard unit, here: the refusing btrbk installed, executable" "$(stat -c %a "$G/run/das-vm-guard/btrbk")" "755"
rm -f "$G/usr/bin/btrbk"
: >"$G/usr/bin/btrbk"
: >"$G/usr/local/sbin/btrbk"
ln -sfn "$G/usr/bin/btrbk" "$G/usr/local/bin/btrbk"
check "guard unit, here: ...over each one present, once" "$(guest_bind)" "rc=0 --bind $G/run/das-vm-guard/btrbk $G/usr/bin/btrbk|--bind $G/run/das-vm-guard/btrbk $G/usr/local/sbin/btrbk|"
rm -f "$G/usr/bin/btrbk" "$G/usr/local/bin/btrbk" "$G/usr/local/sbin/btrbk"
: >"$G/usr/bin/btrbk"
echo 32 >"$G/mount.rc"
check "guard unit, here: a bind that fails fails the guard" "$(guest_bind)" "rc=1 --bind $G/run/das-vm-guard/btrbk $G/usr/bin/btrbk|"
rm -f "$G/mount.rc" "$G/usr/bin/btrbk"
check "guard unit, here: no btrbk, nothing bound" "$(guest_bind)" "rc=0 "
rm -f "$G/creds/das-vm-guard.btrbk"
: >"$G/usr/bin/btrbk"
check "guard unit, here: no refusing btrbk to bind, the guard fails" "$(rm -f "$G/run/das-vm-guard/btrbk"; env GUEST="$G" CREDENTIALS_DIRECTORY="$G/creds" sh -c "$install_line" >/dev/null 2>&1; echo "rc=$?")" "rc=1"
rm -f "$G/usr/bin/btrbk"
credential "$S/defined.at_start.xml" das-vm-guard.btrbk >"$G/creds/das-vm-guard.btrbk"

# The refusing btrbk itself: says so on the kernel log, exits nonzero.
refuser="$(guestify "$(credential "$S/defined.at_start.xml" das-vm-guard.btrbk)")"
: >"$KMSG"
check "the refusing btrbk: exits nonzero" "$(sh -c "$refuser" btrbk run --progress >/dev/null 2>&1; echo $?)" "1"
has "the refusing btrbk: logs the attempt on the kernel log" "$(file "$KMSG")" "das-vm-guard: refused: btrbk run --progress"
has "the refusing btrbk: tells whoever ran it" "$(sh -c "$refuser" btrbk run 2>&1 >/dev/null || :)" "das-vm-guard: btrbk cannot run in this VM session"
# A caller who cannot write the kernel log (any non-root user, in the real VM)
# is told the refusal and nothing else -- not the failed redirect's own error
# (bd DAS-Backup-Manager-uo39). A path in a missing directory fails for root
# too, so this holds however the suite is run.
unwritable="${refuser//"$KMSG"/"$G/no-such-dir/kmsg"}"
check "the refusing btrbk: an unwritable kernel log is not the caller's error" "$(sh -c "$unwritable" btrbk run 2>&1 >/dev/null || :)" "das-vm-guard: btrbk cannot run in this VM session -- lift the guard first: systemctl stop das-vm-guard"
check "the refusing btrbk: ...and still refuses" "$(sh -c "$unwritable" btrbk run >/dev/null 2>&1; echo $?)" "1"

script="$(guestify "$(sed -n "s|^ExecStart=/usr/bin/sh -c '\(.*\)'$|\1|p" <<<"$r")")"
check "reporter: its script is shell" "$(sh -n -c "$script" 2>&1 && echo ok)" "ok"
# Properties from files where one is given, else a guard that holds.
cat >"$G/bin/systemctl" <<'STUB'
#!/bin/bash
[[ ! -f "$GUEST/systemctl.fail" ]] || exit 1
prop=$3 unit=${!#}
[[ ! -f "$GUEST/systemctl.fail.$prop" ]] || exit 1
if [[ -f "$GUEST/$prop.$unit" ]]; then cat "$GUEST/$prop.$unit"; exit 0; fi
case "$prop" in
    ActiveState) echo active ;;
    LoadState) echo bad-setting ;;
    FragmentPath) echo "/run/systemd/generator.early/$unit" ;;
    DropInPaths) echo "/etc/systemd/system/$unit.d/override.conf /run/systemd/generator.early/$unit.d/zzzzzzzz-das-vm-guard.conf" ;;
esac
STUB
# Every mount point, one a line (findmnt -rn -o TARGET): those in $GUEST/mounts.
cat >"$G/bin/findmnt" <<'STUB'
#!/bin/bash
[[ "$*" == "-rn -o TARGET" ]] || { echo "stub findmnt: unexpected $*" >&2; exit 99; }
[[ ! -f "$GUEST/findmnt.fail" ]] || exit 1
echo /
cat "$GUEST/mounts" 2>/dev/null || :
STUB
# readlink, failing when told to.
cat >"$G/bin/readlink" <<'STUB'
#!/bin/bash
[[ ! -f "$GUEST/readlink.fail" ]] || exit 1
exec /usr/bin/readlink "$@"
STUB
# 1 s (the wait for the port): nothing. Longer (a heartbeat, a retry): end
# the reporter after this round -- or, while $GUEST/more counts down, let it
# go round again.
cat >"$G/bin/sleep" <<'STUB'
#!/bin/sh
echo "$1" >>"$GUEST/sleeps"
[ "$1" = 1 ] && exit 0
if [ -f "$GUEST/more" ] && [ "$(cat "$GUEST/more")" -gt 0 ]; then
    echo $(($(cat "$GUEST/more") - 1)) >"$GUEST/more"
    exit 0
fi
kill -TERM "$PPID"
STUB
chmod +x "$G/bin/systemctl" "$G/bin/findmnt" "$G/bin/readlink" "$G/bin/sleep"
echo "$BOOT_A" >"$G/proc/sys/kernel/random/boot_id"
# The refusing btrbk as the guard installs it, and a bind of it over $1.
install -D -m 0755 "$G/creds/das-vm-guard.btrbk" "$G/run/das-vm-guard/btrbk"
bindit() {
    rm -f "$1"
    ln "$G/run/das-vm-guard/btrbk" "$1"
    echo "$1" >>"$G/mounts"
}
unmount() { grep -vxF -- "$1" "$G/mounts" >"$G/mounts.new" || :; mv "$G/mounts.new" "$G/mounts"; }
# One run of the reporter, its state in /run gone first (a new boot), unless
# KEEP_STATE says the boot goes on (the reporter restarted).
guest_report() {
    rm -f "$PORT" "$G/sleeps"
    [[ -n "${KEEP_STATE:-}" ]] || rm -f "$G/run/das-vm-guard/report.state"
    : >"$PORT"
    env GUEST="$G" PATH="$G/bin:$PATH" timeout 10 sh -c "$script" >"$G/stdout" 2>&1
    cat "$PORT"
}
# The refusing btrbk bound over btrbk: the path is its file, and mounted.
rm -f "$G/usr/local/bin/btrbk" "$G/usr/local/sbin/btrbk"
bindit "$G/usr/bin/btrbk"
check "reporter: the btrbk it covers is there (no vacuous engaged)" "$( [ -e "$G/usr/bin/btrbk" ] && [ "$G/usr/bin/btrbk" -ef "$G/run/das-vm-guard/btrbk" ] && echo bound)" "bound"
check "reporter: engaged, with every mask" "$(guest_report)" "$(line "$ENG4")"
check "reporter: ...and into its journal" "$(file "$G/stdout")" "$(line "$ENG4")"
check "reporter: then a heartbeat a minute later" "$(file "$G/sleeps")" "60"
echo inactive >"$G/ActiveState.das-vm-guard.service"
check "reporter: inactive at the start of a boot is not lifted" "$(guest_report)" "$(line "das-vm-guard NOT engaged: das-vm-guard.service is inactive;")"
for st in failed deactivating; do
    echo "$st" >"$G/ActiveState.das-vm-guard.service"
    case $st in
        failed) check "reporter: a guard that failed is said" "$(guest_report)" "$(line "das-vm-guard NOT engaged: das-vm-guard.service is failed;")" ;;
        *) check "reporter: a guard in between says nothing, and looks again" "$(guest_report)$(file "$G/sleeps")" "5" ;;
    esac
done
rm -f "$G/ActiveState.das-vm-guard.service"
echo masked >"$G/LoadState.btrbk.timer"
check "reporter: a unit masked outright counts" "$(guest_report)" "$(line "$ENG4")"
echo loaded >"$G/LoadState.btrbk.timer"
check "reporter: a unit made whole by a drop-in, the condition still last, counts" "$(guest_report)" "$(line "$ENG4")"
echo "/run/systemd/generator.early/btrbk.timer.d/zzzzzzzz-das-vm-guard.conf /etc/systemd/system/btrbk.timer.d/zzzzzzzzz-later.conf" >"$G/DropInPaths.btrbk.timer"
check "reporter: ...but not with a drop-in after the condition" "$(guest_report)" "$(line "das-vm-guard NOT engaged: btrbk.timer is not masked (loaded);")"
echo "/etc/systemd/system/btrbk.timer.d/override.conf" >"$G/DropInPaths.btrbk.timer"
check "reporter: ...nor without the condition" "$(guest_report)" "$(line "das-vm-guard NOT engaged: btrbk.timer is not masked (loaded);")"
rm -f "$G/DropInPaths.btrbk.timer"
echo /usr/lib/systemd/system/btrbk.timer >"$G/FragmentPath.btrbk.timer"
check "reporter: ...nor when the empty unit did not apply" "$(guest_report)" "$(line "das-vm-guard NOT engaged: btrbk.timer is not masked (loaded);")"
rm -f "$G/FragmentPath.btrbk.timer"
echo not-found >"$G/LoadState.crond.service"
check "reporter: a unit not found is not masked (the credential never applied)" "$(guest_report)" "$(line "das-vm-guard NOT engaged: crond.service is not masked (not-found);")"
rm -f "$G"/LoadState.*
echo not-found >"$G/LoadState.btrbk.service"
check "reporter: ...the first one too" "$(guest_report)" "$(line "das-vm-guard NOT engaged: btrbk.service is not masked (not-found);")"
rm -f "$G"/LoadState.*
touch "$G/systemctl.fail"
check "reporter: systemd cannot be asked (a re-exec), says nothing, looks again" "$(guest_report)$(file "$G/sleeps")" "5"
rm -f "$G/systemctl.fail"
for prop in ActiveState LoadState FragmentPath DropInPaths; do
    touch "$G/systemctl.fail.$prop"
    check "reporter: $prop cannot be asked for, says nothing, looks again" "$(guest_report)$(file "$G/sleeps")" "5"
    rm -f "$G/systemctl.fail.$prop"
done
: >"$G/usr/local/bin/btrbk"
check "reporter: a btrbk nothing is bound over is said" "$(guest_report)" "$(line "das-vm-guard NOT engaged: $G/usr/local/bin/btrbk is not covered;")"
echo "$G/usr/local/bin/btrbk" >>"$G/mounts"
check "reporter: a btrbk something else is bound over is said" "$(guest_report)" "$(line "das-vm-guard NOT engaged: $G/usr/local/bin/btrbk is not covered;")"
rm -f "$G/usr/local/bin/btrbk"
unmount "$G/usr/local/bin/btrbk"
ln "$G/run/das-vm-guard/btrbk" "$G/usr/local/bin/btrbk"
check "reporter: the refusing btrbk there without a mount is said" "$(guest_report)" "$(line "das-vm-guard NOT engaged: $G/usr/local/bin/btrbk is not covered;")"
bindit "$G/usr/local/bin/btrbk"
check "reporter: both covered, engaged" "$(guest_report)" "$(line "$ENG4")"
rm -f "$G/usr/local/bin/btrbk"
unmount "$G/usr/local/bin/btrbk"
ln -sfn "$G/usr/bin/btrbk" "$G/usr/local/bin/btrbk"
check "reporter: a btrbk that leads to a covered one is covered" "$(guest_report)" "$(line "$ENG4")"
rm -f "$G/usr/local/bin/btrbk"
check "reporter: no port, nothing written, and no hang" "$(rm -f "$PORT" "$G/sleeps"; env GUEST="$G" PATH="$G/bin:$PATH" timeout 10 sh -c "$script" >/dev/null 2>&1; echo "rc=$? $(ls "$PORT" 2>/dev/null)")" "rc=0 "
check "reporter: waits for the port two minutes, a second at a time" "$(grep -c '^1$' "$G/sleeps")" "120"
# Round 3: the reporter goes on from its state in /run, and names the masks by
# their digest once it has said them in full.
echo 1 >"$G/more"
check "reporter: a heartbeat names the masks by their digest" "$(guest_report)" "$(line "$ENG4")
$(line "$ENG4C" "$BOOT_A" 2)"
check "reporter: ...the first 16 hex digits of the SHA-256 of their list, computed on the host" "${ENG4C##* sha256:}" "$(printf '%s' btrbk.service,btrbk.timer,cronie.service,crond.service | sha256sum | cut -c1-16)"
check "reporter: its state in /run after each line sent: boot, number, engaged seen" "$(file "$G/run/das-vm-guard/report.state")" "$BOOT_A 2 1"
rm -f "$G/more"
# Restarted while lifted: goes on with "lifted" and the boot's numbers, never
# a first-line NOT engaged.
echo inactive >"$G/ActiveState.das-vm-guard.service"
echo "$BOOT_A 3 1" >"$G/run/das-vm-guard/report.state"
check "reporter: restarted while the guard is lifted: lifted, its numbers going on" "$(KEEP_STATE=1 guest_report)" "$(line "$LIFT4" "$BOOT_A" 4)"
echo "$BOOT_B 3 1" >"$G/run/das-vm-guard/report.state"
check "reporter: ...but a state of another boot is not this boot's" "$(KEEP_STATE=1 guest_report)" "$(line "das-vm-guard NOT engaged: das-vm-guard.service is inactive;")"
echo "$BOOT_A 3 0" >"$G/run/das-vm-guard/report.state"
check "reporter: ...nor one that never saw the guard engaged" "$(KEEP_STATE=1 guest_report)" "$(line "das-vm-guard NOT engaged: das-vm-guard.service is inactive;" "$BOOT_A" 4)"
rm -f "$G/ActiveState.das-vm-guard.service"
# What cannot be checked this round is skipped, never "not covered".
touch "$G/findmnt.fail"
check "reporter: findmnt fails, says nothing, looks again" "$(guest_report)$(file "$G/sleeps")" "5"
rm -f "$G/findmnt.fail"
touch "$G/readlink.fail"
check "reporter: readlink fails, says nothing, looks again" "$(guest_report)$(file "$G/sleeps")" "5"
rm -f "$G/readlink.fail"
check "reporter: and with both answering, engaged again" "$(guest_report)" "$(line "$ENG4")"

# Lifted (stopped after the boot's first report): masks still checked, the
# binds not, and said as such. The second round of one run.
cat >"$G/bin/sleep" <<'STUB'
#!/bin/sh
echo "$1" >>"$GUEST/sleeps"
[ "$1" = 1 ] && exit 0
if [ -f "$GUEST/lift_next" ]; then
    rm -f "$GUEST/lift_next" "$GUEST/usr/bin/btrbk" "$GUEST/mounts"
    : >"$GUEST/usr/bin/btrbk"
    echo inactive >"$GUEST/ActiveState.das-vm-guard.service"
    exit 0
fi
kill -TERM "$PPID"
STUB
touch "$G/lift_next"
check "reporter: engaged, then lifted and unbound: both lines, the binds not looked at once lifted" "$(guest_report)" "$(line "$ENG4")
$(line "$LIFT4" "$BOOT_A" 2)"
rm -f "$G/ActiveState.das-vm-guard.service"

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
# The hatch reads its own record, and nothing else: the usual one is of
# another filesystem, refused whatever the options.
write_state 3 "$(record_json system-recovery-A-2tb no 3600 '"eeeeeeee-0000-4000-8000-000000000000"')"
hatch_state no
run_driver session system-recovery-A-2tb --dry-run
check "hatch: DAS_RECOVERY_OS_STATE honoured" "$RC" "0"
has "hatch: says which record it read" "$OUT" "boot record for system-recovery-A-2tb ($T/hatch/recovery-os.json, schema 3)"
printf '{"schema_version":3,"drives":{%s}}\n' "$(record_json system-recovery-A-2tb no 3600 '"ffffffff-0000-4000-8000-000000000000"')" >"$T/hatch/recovery-os.json"
run_driver session system-recovery-A-2tb --dry-run
check "hatch, df0: a record of another filesystem refused" "$RC" "1"
has "hatch, df0: says which" "$OUT" "was read from filesystem ffffffff-0000-4000-8000-000000000000, not $UUID_A"
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
has "define new: validated against the schema" "$(file "$S/virsh.calls")" "define --validate $STATE/system-recovery-A-2tb.plain.xml"
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
has "define over a stored os-level boot: says it updates" "$OUT" "updating recovery-os-updater-system-recovery-A-2tb (for system-recovery-A-2tb) from $T/usr/lib/das-backup/libvirt/recovery-os-updater.xml"
has "define over a stored os-level boot: redefined from the template" "$(file "$S/virsh.calls")" \
    "define --validate $STATE/system-recovery-A-2tb.plain.xml"
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
run_driver screenshot A "$T/shot.png"
check "screenshot while shut off: refused" "$RC" "1"
printf 'running\n' >"$S/states"
run_driver screenshot A "$T/shot.png"
check "screenshot while running: exit 0" "$RC" "0"
has "screenshot: converted to PNG" "$(file "$S/magick.calls")" "png:$T/shot.png"
check "screenshot: written" "$(file "$T/shot.png")" "PNG"

echo "--- define: one domain per recovery drive (7wb, 2026-10-04), the shared one retired"
fixture
rm -f "$S/defined" "$S/domB/defined"
run_driver define
check "define per drive: exit 0" "$RC" "0"
has "define per drive: A's from its rendering" "$(file "$S/virsh.calls")" "define --validate $STATE/system-recovery-A-2tb.plain.xml"
has "define per drive: B's from its rendering" "$(file "$S/virsh.calls")" "define --validate $STATE/system-recovery-B-2tb.plain.xml"
check "define per drive: A's is the template with A's identity" "$(file "$S/defined.xml")" "$(rendered A)"
check "define per drive: B's is the template with B's identity" "$(file "$S/domB/defined.xml")" "$(rendered B)"
lacks "define per drive: their MACs differ (two NICs on one network)" "$(grep -h '<mac ' "$S/defined.xml" "$S/domB/defined.xml" | sort | uniq -d)" "mac"
check "define per drive: no rendering left behind" "$(find "$STATE" -mindepth 1 -maxdepth 1 -name '*plain.xml*' | wc -l)" "0"
lacks "define per drive: no shared domain touched" "$(file "$S/virsh.calls")" "undefine"
check "define per drive: nothing unexpected" "$(file "$S/forbidden")" ""

legacy() {
    mkdir -p "$S/legacy"
    touch "$S/legacy/defined"
    printf 'shut off\n' >"$S/legacy/states"
}
fixture
legacy
run_driver define
check "legacy retired: exit 0" "$RC" "0"
has "legacy retired: undefined with its NVRAM" "$(file "$S/virsh.calls")" "undefine --nvram recovery-os-updater"
check "legacy retired: gone" "$([[ -f "$S/legacy/defined" ]] && echo defined || echo gone)" "gone"
has "legacy retired: said" "$OUT" "retired the shared domain recovery-os-updater"
check "legacy retired: after both drives' domains were defined" "$(calls_in_order "define --validate $STATE/system-recovery-A-2tb.plain.xml" "define --validate $STATE/system-recovery-B-2tb.plain.xml" "undefine --nvram recovery-os-updater")" "yes"
for how in running disk guard snapshots managedsave; do
    fixture
    legacy
    case "$how" in
        running) printf 'running\n' >"$S/legacy/states" ;;
        disk) printf "<disk type='block' device='disk'>\n  <source dev='%s'/>\n</disk>\n" "$DISK_A" >"$S/legacy/attached.xml" ;;
        guard) printf "<domain>\n  <name>recovery-os-updater</name>\n  <devices>\n    <channel type='file'><target type='virtio' name='org.dasbackup.guard'/></channel>\n  </devices>\n</domain>\n" >"$S/legacy/defined.xml" ;;
        snapshots) printf 'pristine\n' >"$S/legacy/snapshots" ;;
        managedsave) echo yes >"$S/legacy/managed_save" ;;
    esac
    run_driver define
    check "legacy $how: define refused" "$RC" "1"
    lacks "legacy $how: nothing defined" "$(file "$S/virsh.calls")" "define --validate"
    lacks "legacy $how: not undefined" "$(file "$S/virsh.calls")" "undefine"
    check "legacy $how: still there" "$([[ -f "$S/legacy/defined" ]] && echo defined || echo gone)" "defined"
done
fixture
legacy
printf 'pristine\n' >"$S/legacy/snapshots"
run_driver define
has "legacy with snapshots: says so" "$OUT" "has libvirt snapshots (pristine) -- define never retires a domain that has them"
fixture
legacy
touch "$S/legacy/undefine_fail"
run_driver define
check "legacy undefine fails: refused" "$RC" "1"
has "legacy undefine fails: the per-drive domains are defined, run again" "$OUT" "the per-drive domains are defined; run define again"
fixture
printf 'running\n' >"$S/domB/states"
run_driver define
check "a drive's domain running: define refused" "$RC" "1"
lacks "a drive's domain running: not even A's redefined" "$(file "$S/virsh.calls")" "define --validate"
fixture
sed -i 's|^  <uuid>.*</uuid>$||' "$T/usr/lib/das-backup/libvirt/recovery-os-updater.xml"
run_driver define
check "a template without a uuid line: refused" "$RC" "1"
has "a template without a uuid line: says which line" "$OUT" "is not of the shape this script renders each drive's domain from"

echo "--- each drive's session uses its own domain"
fixture
run_driver session B
check "session B: exit 0" "$RC" "0"
has "session B: B's domain started" "$(file "$S/virsh.calls")" "start --paused --force-boot recovery-os-updater-system-recovery-B-2tb"
lacks "session B: A's domain never started" "$(file "$S/virsh.calls")" "start --paused --force-boot recovery-os-updater-system-recovery-A-2tb"
has "session B: its events on B's domain" "$(tr '\n' '|' <"$S/domB/events")" "virsh start|"
lacks "session B: none on A's" "$(events)" "virsh start"
has "session B: watched for resets on its own domain" "$(file "$S/virsh.calls")" "event --domain recovery-os-updater-system-recovery-B-2tb --event reboot --loop"
check "session B: nothing unexpected" "$(file "$S/forbidden")" ""

echo "--- dmxt: a session that never resumed records no session time"
fixture
mkdir -p "$STATE"
touch "$S/start_error_but_started"
run_driver session A
check "never resumed (start said it failed, paused): exit 1" "$RC" "1"
check "never resumed: no session time" "$(file "$SESSIONS")" ""
rm -f "$S/start_error_but_started"
run_driver session A
check "never resumed: the retry runs to its end" "$RC" "0"
matches "never resumed: the retry's session recorded" "$(file "$SESSIONS")" "^system-recovery-A-2tb [0-9]+$"
# session-end of a session whose state says it started, never resumed: no time.
fixture
mkdir -p "$STATE"
printf 'verdict=no\nengaged=das-vm-guard engaged 4 masks x\nlifted=das-vm-guard lifted 4 masks x\nengaged_short=das-vm-guard engaged 4 masks sha256:0\nlifted_short=das-vm-guard lifted 4 masks sha256:0\nstarted=%s\n' "$(date +%s)" >"$STATE/system-recovery-A-2tb.guard.state"
printf "<disk type='block' device='disk'>\n  <source dev='%s'/>\n</disk>\n" "$DISK_A" >"$S/attached.xml"
run_driver session-end A
check "session-end, never resumed: exit 0" "$RC" "0"
check "session-end, never resumed: no session time" "$(file "$SESSIONS")" ""
fixture
mkdir -p "$STATE"
printf "<disk type='block' device='disk'>\n  <source dev='%s'/>\n</disk>\n" "$DISK_A" >"$S/attached.xml"
run_driver session-end A
check "session-end, nothing says it never ran: exit 0" "$RC" "0"
matches "session-end, nothing says it never ran: the time recorded (the cautious side)" "$(file "$SESSIONS")" "^system-recovery-A-2tb [0-9]+$"

echo "--- the egress rule (decision 5): the VM's subnet direct, for the session only"
fixture
run_driver session A
check "egress: exit 0" "$RC" "0"
has "egress: the subnet from libvirt's network" "$(file "$S/ip.calls")" "rule add from 192.168.122.0/24 lookup main priority 5100"
check "egress: taken out at the end" "$(file "$S/ip.rules")" ""
has "egress: deleted by the same selector" "$(file "$S/ip.calls")" "rule del from 192.168.122.0/24 lookup main priority 5100"
check "egress: before the start" "$(calls_in_order "net-dumpxml default" "start --paused")" "yes"
has "egress: in the summary" "$OUT" "Egress rule   taken out"
check "egress: no note left" "$([[ -e "$STATE/egress.rule" ]] && echo left || echo gone)" "gone"
fixture
run_driver session A --dry-run
check "egress, dry run: no rule touched" "$(file "$S/ip.calls")" ""
for how in net_fail ip_add_fail ip_add_noop ip_show_fail; do
    fixture
    touch "$S/$how"
    run_driver session A
    check "egress $how: refused" "$RC" "1"
    lacks "egress $how: nothing booted" "$(events)" "virsh start"
    check "egress $how: lock free" "$(lock_state)" "free"
    check "egress $how: disk not left in the definition" "$([[ -f "$S/attached.xml" ]] && echo attached || echo detached)" "detached"
done
fixture
touch "$S/ip_add_noop"
run_driver session A
has "egress, an add that put nothing in place: says so" "$OUT" "no rule from 192.168.122.0/24 is at priority 5100"
fixture
printf "<network>\n  <name>default</name>\n  <ip address='10.1.2.3' prefix='16'>\n  </ip>\n</network>\n" >"$S/net.xml"
run_driver session A
has "egress, a /16 by prefix: its network address" "$(file "$S/ip.calls")" "rule add from 10.1.0.0/16 lookup main priority 5100"
fixture
printf "<network>\n  <ip address='192.168.122.1' netmask='255.255.255.0'/>\n  <ip address='192.168.123.1' netmask='255.255.255.0'/>\n</network>\n" >"$S/net.xml"
run_driver session A
check "egress, two IPv4 addresses: refused" "$RC" "1"
has "egress, two IPv4 addresses: says so" "$OUT" "does not have exactly one IPv4 address"
fixture
printf '192.168.122.0/24\n' >"$S/ip.rules"
run_driver session A
check "egress, one left in place: exit 0" "$RC" "0"
has "egress, one left in place: taken over, said" "$OUT" "already in place, left by a session that did not finish"
lacks "egress, one left in place: not added twice" "$(file "$S/ip.calls")" "rule add"
check "egress, one left in place: taken out at the end" "$(file "$S/ip.rules")" ""
# The add worked and the proof after it could not be read: refused, and the
# rule, owned and noted before the add, is taken out on the way out.
fixture
touch "$S/ip_show_fail_after_add"
run_driver session A
check "egress, unreadable after the add: refused" "$RC" "1"
lacks "egress, unreadable after the add: nothing booted" "$(events)" "virsh start"
check "egress, unreadable after the add: the rule taken out, not left ownerless" "$(file "$S/ip.rules")" ""
has "egress, unreadable after the add: deleted" "$(file "$S/ip.calls")" "rule del from 192.168.122.0/24 lookup main priority 5100"
check "egress, unreadable after the add: no note left" "$([[ -e "$STATE/egress.rule" ]] && echo left || echo gone)" "gone"
# status: a count ip could not read is unknown, never 0.
fixture
mkdir -p "$STATE"
printf '192.168.122.0/24\n' >"$STATE/egress.rule"
printf '192.168.122.0/24\n' >"$S/ip.rules"
run_driver status
has "egress status: the count read" "$OUT" "noted for 192.168.122.0/24 (priority 5100); in place now: 1"
touch "$S/ip_show_fail"
run_driver status
has "egress status, ip fails: unknown" "$OUT" "in place now: unknown (ip rule show failed: RTNETLINK answers: Operation not permitted)"
lacks "egress status, ip fails: never 0" "$OUT" "in place now: 0"
fixture
touch "$S/ip_del_fail"
run_driver session A
check "egress cannot be taken out: exit 5" "$RC" "5"
has "egress cannot be taken out: in the summary" "$OUT" "Warnings      the egress rule was NOT taken out"
check "egress cannot be taken out: still there" "$(file "$S/ip.rules")" "192.168.122.0/24"
check "egress cannot be taken out: the note stays, for session-end" "$(file "$STATE/egress.rule")" "192.168.122.0/24"
# A session left running keeps the rule; session-end takes it out once the
# recovery OS is off.
fixture
printf 'running\n' >"$S/states.running"
run_interrupted session A
check "egress, interrupted: exit 3" "$RC" "3"
check "egress, interrupted: the rule stays while the OS runs" "$(file "$S/ip.rules")" "192.168.122.0/24"
has "egress, interrupted: says session-end takes it out" "$OUT" "The egress rule (if one is in place) and a console bridge stay too"
printf 'shut off\n' >"$S/states"
run_driver session-end A
check "egress, session-end: exit 0" "$RC" "0"
check "egress, session-end: taken out" "$(file "$S/ip.rules")" ""
check "egress, session-end: the note gone" "$([[ -e "$STATE/egress.rule" ]] && echo left || echo gone)" "gone"
fixture
mkdir -p "$STATE"
printf '192.168.122.0/24\n' >"$STATE/egress.rule"
printf '192.168.122.0/24\n' >"$S/ip.rules"
printf 'running\n' >"$S/domB/states"
run_driver session-end A
check "egress, session-end while B's OS runs: left in place for it" "$(file "$S/ip.rules")" "192.168.122.0/24"

echo "--- unattended sessions (8249): only through an agent the record says runs"
# A drive's entry with the guest agent the nightly read records: $1 label, $2
# verdict, $3 the guest_agent JSON.
record_agent() {
    record_json "$1" "$2" | sed "s|\"os\":{|\"os\":{\"guest_agent\":$3,|"
}
AGENT_ON='{"state":"read","installed":true,"enabled":true,"why":"started by usr/lib/udev/rules.d/99-qemu-guest-agent.rules when the agent'"'"'s virtio port appears"}'
unattended_fixture() {
    fixture
    write_state 4 "$(record_agent system-recovery-A-2tb "${1:-no}" "$AGENT_ON")" "$(record_agent system-recovery-B-2tb no "$AGENT_ON")"
    printf 'running\n' >"$S/states.running"
    printf 'running\n' >"$S/domB/states.running"
}
STEPS="egress snapshot guard-lift keyrings upgrade packages initramfs verify-btrbk verify-boot guard-engage"
for how in schema3 not-installed not-enabled unreadable; do
    fixture
    case "$how" in
        schema3) ;;
        not-installed) write_state 4 "$(record_agent system-recovery-A-2tb no '{"state":"read","installed":false,"enabled":false,"why":"qemu-guest-agent is not installed"}')" ;;
        not-enabled) write_state 4 "$(record_agent system-recovery-A-2tb no '{"state":"read","installed":true,"enabled":false,"why":"etc/systemd/system/qemu-guest-agent.service masks or replaces the package'"'"'s unit"}')" ;;
        unreadable) write_state 4 "$(record_agent system-recovery-A-2tb no '{"state":"unreadable","reason":"not read"}')" ;;
    esac
    run_driver session A --unattended --accept-boot-record-risk
    check "unattended, agent $how: refused" "$RC" "1"
    has "unattended, agent $how: says what it needs" "$OUT" "an unattended session needs the recovery OS's QEMU guest agent installed and started at boot"
    check "unattended, agent $how: nothing taken" "$(file "$S/flock.calls")" ""
done
has "unattended, agent unreadable: says what the record says" "$OUT" "unreadable (not read)"
unattended_fixture will
run_driver session A --unattended --accept-boot-record-risk
check "unattended on a will record: refused, whatever the options (decision 3)" "$RC" "1"
has "unattended on a will record: says so" "$OUT" "a \"will\" drive is updated attended only"
check "unattended on a will record: nothing taken" "$(file "$S/flock.calls")" ""

unattended_fixture
run_driver session A --unattended
check "unattended: exit 0" "$RC" "0"
check "unattended: every step, in order" "$(tr '\n' ' ' <"$S/stages.log")" "$STEPS "
has "unattended: the guest rebooted" "$(events)" "guest reboot"
has "unattended: then powered off through its agent" "$(events)" "guest shutdown"
has "unattended: the kernel after the reboot" "$OUT" "PROGRESS system-recovery-A-2tb kernel ok 7.2.9-1-cachyos"
has "unattended: a step's output, a line of its own" "$OUT" "OUTPUT system-recovery-A-2tb upgrade did upgrade"
has "unattended: a step's end" "$OUT" "PROGRESS system-recovery-A-2tb verify-boot ok"
has "unattended: the reboot answered by an engaged boot" "$OUT" "PROGRESS system-recovery-A-2tb reboot ok boot bbbbbbbb-1111-4222-8333-444444444444, its guard engaged"
has "unattended: summary" "$OUT" "Unattended    every step done; rebooted into kernel 7.2.9-1-cachyos"
has "unattended: its result" "$OUT" "RESULT system-recovery-A-2tb 0 clean"
has "unattended: given back" "$(events)" "virsh detach"
check "unattended: no egress rule left" "$(file "$S/ip.rules")" ""
has "unattended: the egress step checks for the org it was given" "$(file "$S/stage.egress.sh")" "want=AS209"
# shellcheck disable=SC2016 # the step's script, matched as written
has "unattended: the snapshot is the guest's own, of its @" "$(file "$S/stage.snapshot.sh")" 'btrfs subvolume snapshot -r "$top/@" "$top/$name"'
has "unattended: keeps the newest two" "$(file "$S/stage.snapshot.sh")" "head -n -2"
has "unattended: the full upgrade" "$(file "$S/stage.upgrade.sh")" "pacman -Syu --noconfirm"
has "unattended: the agent and the microcode" "$(file "$S/stage.packages.sh")" "pacman -S --needed --noconfirm qemu-guest-agent && pacman -S --noconfirm amd-ucode intel-ucode"
has "unattended: the fallback preset, then every image" "$(file "$S/stage.initramfs.sh")" "mkinitcpio -P"
has "unattended: btrbk as packaged" "$(file "$S/stage.verify-btrbk.sh")" "0 altered files"
# shellcheck disable=SC2016 # the step's script, matched as written
has "unattended: the loader's files and fstab's devices" "$(file "$S/stage.verify-boot.sh")" 'findfs "$spec"'
# shellcheck disable=SC2016 # the step's script, matched as written
has "unattended: the guard engaged again, every btrbk bound" "$(file "$S/stage.guard-engage.sh")" 'findmnt -rn --mountpoint "$p"'
check "unattended: nothing unexpected" "$(file "$S/forbidden")" ""
line=$(file "$T/var/lib/das-backup/recovery-os-vm-history.jsonl")
check "unattended: one history line" "$(grep -c . <<<"$line")" "1"
check "unattended: its fields" "$(jq -c '[.label, .mode, .unattended, .outcome, .exit, .overridden, .kernel, .stopped_at]' <<<"$line")" \
    '["system-recovery-A-2tb","single",true,"clean",0,false,"7.2.9-1-cachyos",null]'
check "unattended: start before end" "$(jq '.start <= .end' <<<"$line")" "true"

for step in egress snapshot keyrings upgrade verify-btrbk verify-boot guard-engage; do
    unattended_fixture
    echo 3 >"$S/stage.$step.rc"
    printf '%s refused\n' "$step" >"$S/stage.$step.out"
    run_driver session A --unattended
    check "unattended, $step fails: exit 7" "$RC" "7"
    check "unattended, $step fails: nothing after it ran" "$(tail -n 1 "$S/stages.log")" "$step"
    has "unattended, $step fails: powered off through the agent" "$(events)" "guest shutdown"
    lacks "unattended, $step fails: no reboot" "$(events)" "guest reboot"
    has "unattended, $step fails: given back" "$(events)" "virsh detach"
    has "unattended, $step fails: the step and why" "$OUT" "PROGRESS system-recovery-A-2tb $step fail exit status 3 -- $step refused"
    has "unattended, $step fails: summary" "$OUT" "Unattended    STOPPED at $step: exit status 3"
    check "unattended, $step fails: history" "$(jq -c '[.outcome, .exit, .stopped_at]' "$T/var/lib/das-backup/recovery-os-vm-history.jsonl")" "[\"failed\",7,\"$step\"]"
    check "unattended, $step fails: lock free" "$(lock_state)" "free"
    check "unattended, $step fails: no egress rule left" "$(file "$S/ip.rules")" ""
done
# The egress check's own refusal, as the exit node gives it.
unattended_fixture
echo 3 >"$S/stage.egress.rc"
printf 'egress: 185.0.0.1, AS207990 HostRoyale Technologies\negress is AS207990 HostRoyale Technologies, not AS209: the update would go through another path (a VPN exit node) -- refused\n' >"$S/stage.egress.out"
run_driver session A --unattended
check "unattended, egress via the exit node: exit 7" "$RC" "7"
has "unattended, egress via the exit node: said" "$OUT" "OUTPUT system-recovery-A-2tb egress egress: 185.0.0.1, AS207990 HostRoyale Technologies"
check "unattended, egress via the exit node: pacman never ran" "$(grep -c 'keyrings\|upgrade' "$S/stages.log" || :)" "0"

unattended_fixture
touch "$S/stage.keyrings.launch_fails"
run_driver session A --unattended
check "unattended, a step that cannot start: exit 7" "$RC" "7"
has "unattended, a step that cannot start: said" "$OUT" "it could not be started in the recovery OS (exit status 1"
unattended_fixture
touch "$S/stage.upgrade.hangs"
DAS_RECOVERY_VM_UPDATE_SECS=1 run_driver session A --unattended
check "unattended, a step past its limit: exit 7" "$RC" "7"
has "unattended, a step past its limit: said" "$OUT" "PROGRESS system-recovery-A-2tb upgrade fail not done within 0h 00m 01s"
unattended_fixture
touch "$S/agent_down"
printf 'shut off\n' >"$S/states.after_shutdown"
DAS_RECOVERY_VM_AGENT_SECS=1 run_driver session A --unattended
check "unattended, no agent: exit 7" "$RC" "7"
has "unattended, no agent: stopped at boot" "$OUT" "PROGRESS system-recovery-A-2tb boot fail the guest agent did not answer within"
check "unattended, no agent: no step ran" "$(file "$S/stages.log")" ""
has "unattended, no agent: ACPI asks it off" "$(events)" "virsh shutdown"
unattended_fixture may
touch "$S/reboot_unguarded"
printf 'shut off\n' >"$S/states.after_shutdown"
run_driver session A --unattended
check "unattended, a reboot without the guard on a may record: exit 6" "$RC" "6"
has "unattended, a reboot without the guard: stopped at the reboot" "$OUT" "PROGRESS system-recovery-A-2tb reboot fail the session guard did not confirm"
unattended_fixture
touch "$S/stage.upgrade.hangs"
run_driver session A --unattended --timeout 1
check "unattended, --timeout: exit 7" "$RC" "7"
has "unattended, --timeout: said" "$OUT" "the --timeout of 1 minute(s) has passed"
# A step that changes the OS, stopped: the power-off may end pacman
# mid-transaction, so the summary says so and names the way back.
for step in keyrings upgrade packages initramfs; do
    unattended_fixture
    printf 'snapshot: @.pre-update.20261007-1200 (read-only)\n' >"$S/stage.snapshot.out"
    echo 1 >"$S/stage.$step.rc"
    run_driver session A --unattended
    check "unattended, $step stopped: exit 7" "$RC" "7"
    has "unattended, $step stopped: at that step" "$OUT" "PROGRESS system-recovery-A-2tb $step fail exit status 1"
    has "unattended, $step stopped: half-upgraded, said in the summary" "$OUT" "  Unattended    WARNING: the recovery OS may be half-upgraded ('$step' may have been ended by the power-off"
    has "unattended, $step stopped: the snapshot to roll back to, named" "$OUT" "roll back to @.pre-update.20261007-1200 (read-only"
done
# The case it is for: pacman past its limit, still running at the power-off.
unattended_fixture
printf 'snapshot: @.pre-update.20261007-1200 (read-only)\n' >"$S/stage.snapshot.out"
touch "$S/stage.upgrade.hangs"
DAS_RECOVERY_VM_UPDATE_SECS=1 run_driver session A --unattended
has "unattended, upgrade past its limit: half-upgraded, the snapshot named" "$OUT" "  Unattended    WARNING: the recovery OS may be half-upgraded ('upgrade' may have been ended by the power-off in the middle of its work): roll back to @.pre-update.20261007-1200"
unattended_fixture
touch "$S/stage.upgrade.hangs"
DAS_RECOVERY_VM_UPDATE_SECS=1 run_driver session A --unattended
has "unattended, upgrade stopped, no snapshot name read: still said" "$OUT" "roll back to the @.pre-update.<YYYYMMDD-HHMM> snapshot this run took"
for step in snapshot verify-boot; do
    unattended_fixture
    echo 3 >"$S/stage.$step.rc"
    run_driver session A --unattended
    lacks "unattended, $step fails: nothing changed the OS, no half-upgrade said" "$OUT" "half-upgraded"
done

echo "--- the session history and the clean-run count (decision 7)"
fixture
run_driver clean-runs A
check "clean-runs, no history: 0" "$(tail -n 1 <<<"$OUT")" "0"
check "clean-runs, no history: exit 0" "$RC" "0"
hl() { printf '{"label":"%s","start":1,"end":2,"mode":"single","unattended":%s,"outcome":"%s","exit":0,"overridden":%s,"kernel":null,"stopped_at":null}\n' "$1" "$2" "$3" "$4"; }
{
    hl system-recovery-A-2tb true clean false
    hl system-recovery-A-2tb true failed false
    hl system-recovery-A-2tb true clean false
    hl system-recovery-B-2tb true failed false
    hl system-recovery-A-2tb false clean false
    hl system-recovery-A-2tb true clean false
} >"$H"
run_driver clean-runs A
check "clean-runs: counted since the last failure, an attended clean one neither counted nor resetting" "$(tail -n 1 <<<"$OUT")" "2"
run_driver clean-runs system-recovery-B-2tb
check "clean-runs: another drive's failure is its own" "$(tail -n 1 <<<"$OUT")" "0"
hl system-recovery-A-2tb true clean true >>"$H"
run_driver clean-runs A
check "clean-runs: an overridden session resets it" "$(tail -n 1 <<<"$OUT")" "0"
hl system-recovery-A-2tb true warnings false >>"$H"
hl system-recovery-A-2tb true clean false >>"$H"
run_driver clean-runs A
check "clean-runs: one with warnings resets it too" "$(tail -n 1 <<<"$OUT")" "1"
run_driver history A
check "history A: A's lines only" "$(grep -c '"system-recovery-A-2tb"' <<<"$OUT")" "8"
lacks "history A: not B's" "$OUT" "system-recovery-B-2tb"
printf 'not json\n' >>"$H"
run_driver clean-runs A
check "clean-runs, a damaged line: refused, never a guess" "$RC" "1"
has "clean-runs, a damaged line: says so" "$OUT" "cannot be read as the session history"
fixture
run_driver session A
check "history, an attended session: exit 0" "$RC" "0"
check "history, an attended session: written" "$(jq -c '[.unattended, .outcome, .mode]' "$H")" '[false,"clean","single"]'
run_driver session A --dry-run --accept-boot-record-risk
check "history, a dry run: nothing written" "$(grep -c . "$H")" "1"
fixture
touch "$S/start_fail"
run_driver session A
check "history, a failed session: failed" "$(jq -c '[.outcome, .exit]' "$H")" '["failed",1]'
fixture
start_blocker
run_driver session A
stop_blocker
check "history, refused before the lock: nothing written" "$(file "$H")" ""

echo "--- both drives in one run: one lock, sequential or parallel"
unattended_fixture
run_driver session A B --unattended
check "both, sequential (the default): exit 0" "$RC" "0"
has "both: A's result" "$OUT" "DRIVE system-recovery-A-2tb 0"
has "both: B's result" "$OUT" "DRIVE system-recovery-B-2tb 0"
check "both, sequential: A's steps, then B's" "$(tr '\n' ' ' <"$S/stages.log")" "$STEPS $STEPS "
has "both: A's lines carry its label" "$OUT" "[system-recovery-A-2tb] took the session guard out"
has "both: B's too" "$OUT" "[system-recovery-B-2tb] took the session guard out"
has "both: each drive holds the run's lock" "$OUT" "holding the DAS maintenance lock $LOCK through the two-drive run that took it"
lacks "both: no drive took the lock itself" "$OUT" "] took the DAS maintenance lock"
check "both: the run took it, once" "$(grep -c 'took the DAS maintenance lock' <<<"$OUT")" "1"
check "both: the lock's record names the run while it runs" "$(sort -u "$S/lock.records" | grep -c 'recovery-os VM session system-recovery-A-2tb+system-recovery-B-2tb pid ')" "1"
check "both: lock free at the end" "$(lock_state)" "free"
check "both: the egress rule put in place once" "$(grep -c 'rule add' "$S/ip.calls")" "1"
check "both: and taken out" "$(file "$S/ip.rules")" ""
check "both: two history lines, sequential" "$(jq -r '.mode' "$H" | sort | uniq -c | tr -s ' ')" " 2 sequential"
check "both: nothing unexpected" "$(file "$S/forbidden")" ""

unattended_fixture
echo 3 >"$S/stage.upgrade.rc"
run_driver session A B --unattended --mode sequential
check "both, A fails: exit 7" "$RC" "7"
has "both, A fails: B not started" "$OUT" "DRIVE system-recovery-B-2tb skipped"
lacks "both, A fails: B's domain never started" "$(file "$S/domB/events")" "virsh start"
has "both, A fails: said why" "$OUT" "is not started: system-recovery-A-2tb exited 7"
check "both, A fails: lock free" "$(lock_state)" "free"

unattended_fixture
run_driver session A B --unattended --mode parallel
check "both, parallel: exit 0" "$RC" "0"
check "both, parallel: both drives' steps ran" "$(sort "$S/stages.log" | uniq -c | awk '{print $1}' | sort -u)" "2"
has "both, parallel: A started" "$(events)" "virsh start"
has "both, parallel: B started" "$(file "$S/domB/events")" "virsh start"
check "both, parallel: two history lines" "$(jq -r '.mode' "$H" | sort | uniq -c | tr -s ' ')" " 2 parallel"
check "both, parallel: lock free" "$(lock_state)" "free"
check "both, parallel: no egress rule left" "$(file "$S/ip.rules")" ""

fixture
run_driver session A system-recovery-A-2tb
check "both, the same drive twice: refused" "$RC" "1"
has "both, the same drive twice: says so" "$OUT" "are the same drive"
fixture
start_blocker
run_driver session A B
stop_blocker
check "both, the lock held by another job: refused" "$RC" "1"
check "both, the lock held: neither drive started" "$(file "$S/virsh.calls" | grep -c 'start --paused' || :)" "0"
fixture
DAS_RECOVERY_VM_LOCK_FD=9 run_driver session A
check "a drive's session with a descriptor that is not the lock: refused" "$RC" "1"
has "...says so" "$OUT" "is not an open descriptor of"
fixture
DAS_RECOVERY_VM_MODE=parallel run_driver session A
check "DAS_RECOVERY_VM_MODE without the run's lock: a usage error" "$RC" "2"

echo "--- the console bridge (decision 2): one user, one connection, the session only"
console_fixture() {
    fixture
    mkdir -p "$STATE" "$T/var/lib/libvirt/qemu/domain-1-x"
    python3 -c 'import socket,sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])' "$T/var/lib/libvirt/qemu/domain-1-x/vnc.sock"
    setsid env STUB="$S" REAL_FLOCK="$REAL_FLOCK" DAS_RECOVERY_VM_TEST_ROOT="$T" \
        "$T/bin/btrdasd" recovery-os hold-disk --device "$DISK_A" </dev/null >"$S/old.out" 2>&1 &
    cholder=$!
    for ((i = 0; i < 200; i++)); do grep -q '^held ' "$S/old.out" 2>/dev/null && break; sleep 0.05; done
    printf '%s\n%s\n' "$cholder" "$DISK_A" >"$STATE/system-recovery-A-2tb.holder"
    printf 'running\n' >"$S/states"
    printf "<domain>\n  <name>recovery-os-updater-system-recovery-A-2tb</name>\n  <devices>\n    <graphics type='vnc' socket='/var/lib/libvirt/qemu/domain-1-x/vnc.sock'>\n      <listen type='socket' socket='/var/lib/libvirt/qemu/domain-1-x/vnc.sock'/>\n    </graphics>\n  </devices>\n</domain>\n" >"$S/defined.xml"
}
me=$(id -u)
console_fixture
run_driver console-socket A "$me"
check "console: exit 0" "$RC" "0"
sock=$(driver_env bash "$DRIVER" console-socket A "$me" 2>/dev/null) || sock=""
matches "console: prints only the socket's path on stdout" "$sock" "^$STATE/console-system-recovery-A-2tb-[A-Za-z0-9]+/vnc\.sock$"
check "console: the socket is the user's, 0600" "$(stat -c '%u %a %F' "$sock" 2>/dev/null)" "$me 600 socket"
check "console: in a directory of root's (the runner's), 0711 -- never the user's" "$(stat -c '%u %a' "$(dirname "$sock")" 2>/dev/null)" "$(id -u) 711"
has "console: the socket's mode asked of socat" "$(tail -n 1 "$S/socat.calls")" ",mode=600,"
has "console: the socket's user asked of socat" "$(tail -n 1 "$S/socat.calls")" ",user=$me,"
check "console: the state directory can be passed through, not listed" "$(stat -c %a "$STATE")" "711"
has "console: proxied to libvirt's VNC socket" "$(file "$S/socat.calls")" "UNIX-CONNECT:$T/var/lib/libvirt/qemu/domain-1-x/vnc.sock"
has "console: one connection (no fork)" "$(tail -n 1 "$S/socat.calls")" "UNIX-LISTEN:$sock,mode=600,user=$me"
lacks "console: never forks" "$(file "$S/socat.calls")" "fork"
check "console: a new bridge replaced the old one" "$(find "$STATE" -mindepth 1 -maxdepth 1 -type d -name 'console-system-recovery-A-2tb-*' | wc -l)" "1"
bpid=$(head -n 1 "$STATE/system-recovery-A-2tb.console")
printf 'shut off\n' >"$S/states"
run_driver session-end A
check "console: session-end exit 0" "$RC" "0"
check "console: removed with the give-back" "$(find "$STATE" -mindepth 1 -maxdepth 1 -name 'console-*' 2>/dev/null | wc -l)" "0"
check "console: its process ended" "$([[ -d "/proc/$bpid" ]] && grep -q socat "/proc/$bpid/cmdline" 2>/dev/null && echo running || echo gone)" "gone"
check "console: its record gone" "$([[ -e "$STATE/system-recovery-A-2tb.console" ]] && echo left || echo gone)" "gone"
kill "$cholder" 2>/dev/null || :
wait "$cholder" 2>/dev/null || :

fixture
run_driver console-socket A "$me"
check "console, no session: refused" "$RC" "1"
has "console, no session: says so" "$OUT" "no session of system-recovery-A-2tb is running"
console_fixture
run_driver console-socket A root-please
check "console, a user that is no number: refused" "$RC" "1"
run_driver console-socket A 4294967290
check "console, a user that does not exist: refused" "$RC" "1"
has "console, a user that does not exist: says so" "$OUT" "no user has the id 4294967290"
printf 'paused\n' >"$S/states"
run_driver console-socket A "$me"
check "console, the domain not running: refused" "$RC" "1"
printf 'running\n' >"$S/states"
touch "$S/socat_fail"
run_driver console-socket A "$me"
check "console, socat fails: refused" "$RC" "1"
check "console, socat fails: nothing left" "$(find "$STATE" -mindepth 1 -maxdepth 1 -name 'console-*' 2>/dev/null | wc -l)" "0"
mkdir -p "$T/nosocat"
for t in bash env timeout readlink dirname basename cat head tr; do ln -sf "$(command -v "$t")" "$T/nosocat/$t"; done
cp "$T/bin/id" "$T/nosocat/id"
DRIVER_PATH="$T/nosocat" run_driver console-socket A "$me"
check "console, no socat: refused" "$RC" "1"
has "console, no socat: says so" "$OUT" "socat is not installed"
kill "$cholder" 2>/dev/null || :
wait "$cholder" 2>/dev/null || :

echo "--- across every test above: destroy is unreachable after a resume (I-1)"
check "no virsh destroy of a domain that was resumed, in any test" "$(grep -c '^destroy after a resume' "$DESTROY_LOG")" "0"
matches "...and the log does see destroys: of domains never resumed, in the tests that need one" "$(grep -c '^destroy, never resumed' "$DESTROY_LOG")" "^[1-9][0-9]*$"

echo
echo "$passes passed, $fails failed"
if ((fails > 0)); then
    exit 1
fi
echo "RECOVERY-OS-VM SUITE GREEN"
