#!/bin/bash
# backup-verify.sh - Verify DAS drive health and backup status (config-driven)
# Version: 3.5.0
# Date: 2026-11-27
#
# 3.5.0: reader-friendly output (bd DAS-Backup-Manager-3gzs.12). A run header
# and one `── item ──` header per section and per drive, and a closing RESULT
# block with one row per drive and one for the backup-status check, the status
# word (OK, WARN, FAIL, SKIPPED) in a fixed column and a clause saying why. The
# time carries its zone. Every message the checks pass to log_info, log_warn and
# log_error, the `Health:` lines and the exit status are unchanged.
#
# 3.4.1: `btrbk list latest` is scoped to the primary's target directories
# (`<mount>/*`); unscoped it failed on the unmounted recovery targets and the
# section read "snapshot status UNKNOWN" on every run (bd
# DAS-Backup-Manager-su1b.23).
#
# 3.4.0: a RAID-1 primary (a filesystem UUID and more than one member serial)
# is mounted through the guard, `btrdasd backup mount-target --read-only`,
# handed the maintenance lock as fd 8, never by a hand `mount ... degraded`
# that could read a stale member's tree and skipped the rejected-member and
# record checks; a single-device primary is mounted read-only without
# `degraded`. A guard failure (refused, not mounted, could not run) is
# reported as a failed mount with the guard's reason (bd
# DAS-Backup-Manager-su1b.23).
#
# 3.3.0: a drive's serial is read by one strict rule (read_drive_serial() over
# parse_drive_serial(), the partition script's): one "Serial Number:" or SAS
# "Serial number:" line, the whole value, smartctl's failure bits refused (bd
# DAS-Backup-Manager-2kb1.17). [das].model_pattern has its blanks removed as
# the model has (docs/INSTALL.md), so a pattern written with a space matches
# (bd DAS-Backup-Manager-2kb1.15).
#
# 3.2.1: the skip line for a held maintenance lock names the holder from its
# record in the lock file, or says plainly that the record is empty or cannot
# be read (maintenance_lock_holder(); bd DAS-Backup-Manager-3wc).
#
# 3.2.0: no command runs at the top level any more. The configuration load,
# the drive map and the globals ran there, before main(), so a copy over this
# file in place while the load ran (the `btrdasd config dump-env` it waits for)
# had bash read the new file's bytes next; they are load_configuration(),
# called by main(), and the file is function definitions and the last line
# (bd DAS-Backup-Manager-896; tests/test_verify_rewrite.sh).
#
# 3.1.3: a sector-count attribute is a number only if it is ASCII digits:
# report_sector_attr() matches [[:digit:]], not [0-9], which under en_US.UTF-8
# also matches digits of other scripts and superscripts, and printed such a
# value as a count in yellow, returning success, where it is a reading no
# one can use (bd DAS-Backup-Manager-1bsx; tests/test_early_exit_readers.sh).
#
# 3.1.2: the SMART health line is matched by bash itself, `[[ ]]`. Under
# pipefail, `echo "$health" | grep -q PASSED` read a PASSED followed by
# more than 64 KiB of output as no match: grep quit at the match and echo
# died of SIGPIPE (bd DAS-Backup-Manager-wkvz). A here-string would need a
# pipe or a temp file, and with no fd to spare it reads "no match" as well.
#
# 3.1.1: the last line is `main "$@"; exit $?`, so a copy over this file
# in place while it runs (a plain `cp`) cannot have bash read the new file
# once main returns; behaviour is otherwise unchanged
# (bd DAS-Backup-Manager-6wt).
#
# 3.1.0: the maintenance lock is taken by take_maintenance_lock(), which
# records this script as its holder, empties that record again on exit while
# the lock is still held, and no longer empties the current holder's record
# on open (bd DAS-Backup-Manager-frb). A btrbk/usage inspection skipped
# because the lock is held is no longer also reported as a failed mount
# (bd DAS-Backup-Manager-0oi).
#
# Checks:
#   - SMART health on all DAS drives
#   - btrbk snapshot status
#   - Disk space usage
#   - All configuration loaded from config.toml via btrdasd
#
# Usage:
#   sudo ./backup-verify.sh          # Full verification
#   sudo ./backup-verify.sh --quick  # SMART only (no btrbk check)

set -euo pipefail
# ============================================================================
# CONFIGURATION (loaded from config.toml via btrdasd)
# ============================================================================
#
# Nothing below runs at the top level: bash reads a script as it runs, so a
# command at the top level that a rewrite of this file (a plain `cp`, `btrdasd
# setup --upgrade`) lands during would have bash read the new file's bytes
# next. The load runs in load_configuration(), called by main(), and the last
# line starts main (bd DAS-Backup-Manager-896; tests/test_verify_rewrite.sh).

load_configuration() {
    # Globals: check_btrbk_status mounts a RAID-1 primary through the same
    # btrdasd and config (bd DAS-Backup-Manager-su1b.23).
    BTRDASD_BIN="${BTRDASD_BIN:-/usr/bin/btrdasd}"
    DAS_CONFIG="${DAS_CONFIG:-/etc/das-backup/config.toml}"
    local btrdasd_bin="$BTRDASD_BIN"
    local das_config="$DAS_CONFIG"
    if [[ -x "$btrdasd_bin" ]]; then
        eval "$("$btrdasd_bin" config dump-env --config "$das_config")"
    else
        echo "ERROR: btrdasd not found at $btrdasd_bin" >&2
        exit 1
    fi

    # Build drive map from config targets (serial -> display name).
    #
    # Keyed on every member serial from DAS_TARGET_n_SERIALS, not just the
    # legacy single-anchor DAS_TARGET_n_SERIAL. A RAID-1 target has two member
    # drives and the anchor names only one of them, so the partner rendered as
    # "Unknown" in the SMART table while its health was being read perfectly
    # well — the map was what was wrong, not the measurement (bd
    # DAS-Backup-Manager-5b1).
    declare -gA DRIVE_MAP=()
    local i serials_var serial_var name_var label_var display serial
    local -a target_serials
    for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
        serials_var="DAS_TARGET_${i}_SERIALS"
        serial_var="DAS_TARGET_${i}_SERIAL"
        name_var="DAS_TARGET_${i}_DISPLAY_NAME"
        label_var="DAS_TARGET_${i}_LABEL"

        display="${!name_var:-}"
        [[ -n "$display" ]] || display="${!label_var:-}"

        # Fall back to the anchor when SERIALS is absent (config written by a
        # pre-0.7.13 btrdasd), so an older config still maps its one drive.
        read -ra target_serials <<< "${!serials_var:-${!serial_var:-}}"
        for serial in "${target_serials[@]}"; do
            [[ -n "$serial" ]] || continue
            DRIVE_MAP[$serial]="$display"
        done
    done

    # Expected DAS drives (detected by USB transport)
    DAS_DEVICES=()

    # Set by check_smart_health when any drive fails or cannot be read. main()
    # exits nonzero on it so this script can be used as a gate.
    SMART_FAILED=false

    # What the closing RESULT block shows: one `<drive>|<status>|<why>` per
    # drive (set by check_smart_health) and the backup-status check's
    # `<status>|<why>` (set by check_btrbk_status, or by main for --quick).
    DRIVE_RESULTS=()
    BTRBK_RESULT="SKIPPED|the backup-status check was not run"

    # Colors
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    NC='\033[0m'
}

# ============================================================================
# FUNCTIONS
# ============================================================================

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

log_header() {
    echo ""
    echo -e "${BLUE}── $1 ──${NC}"
}

# One RESULT row: <what> <status> <why>, the status word in a fixed column.
result_row() {
    printf '  %-34s %-8s %s\n' "$1" "$2" "$3"
}

check_root() {
    if [[ $EUID -ne 0 ]]; then
        log_error "This script must be run as root"
        exit 1
    fi
}

# Read one SMART attribute's raw value (field 10 of the attribute table row).
#
# Prints exactly one of:
#   <value>          the raw value as smartctl reported it
#   NOT_PRESENT      smartctl produced a table, but not this attribute
#   SMARTCTL_FAILED  smartctl produced nothing usable for this device
#
# It must NEVER substitute a zero. `... | grep ATTR | awk '{print $10}' ||
# echo "0"` fabricated a clean sector count whenever the attribute was absent
# or smartctl failed, and that zero was then printed in green — a manufactured
# clean bill of health from the check that decides whether a drive is dying.
smart_attr_raw() {
    local dev="$1" attr="$2"
    local out=""

    # smartctl exits nonzero for real read failures AND for benign health-flag
    # bits, so the presence of output is what distinguishes them.
    out=$(smartctl -A "$dev" 2>/dev/null) || true

    if [[ -z "$out" ]]; then
        echo "SMARTCTL_FAILED"
        return 0
    fi

    # Herestring, not a pipe: awk's early `exit` closes its input, and under
    # `set -o pipefail` a SIGPIPE'd producer would fail the whole assignment.
    local value
    value=$(awk -v a="$attr" '$2 ~ a { print $10; exit }' <<< "$out")

    if [[ -z "$value" ]]; then
        echo "NOT_PRESENT"
        return 0
    fi

    echo "$value"
}

# Print a sector-count attribute. Returns nonzero when the value could not be
# established, so only a real number is ever reported as healthy.
report_sector_attr() {
    local label="$1" value="$2"

    case "$value" in
        SMARTCTL_FAILED)
            echo -e "  $label: ${RED}UNKNOWN (smartctl could not read this device)${NC}"
            return 1
            ;;
        NOT_PRESENT)
            echo -e "  $label: ${RED}UNKNOWN (attribute not reported by this device)${NC}"
            return 1
            ;;
        0)
            echo -e "  $label: ${GREEN}0${NC}"
            return 0
            ;;
        *)
            if [[ "$value" =~ ^[[:digit:]]+$ ]]; then
                echo -e "  $label: ${YELLOW}$value${NC}"
                return 0
            fi
            echo -e "  $label: ${RED}UNKNOWN (unparsable value: $value)${NC}"
            return 1
            ;;
    esac
}

# Render an informational attribute without ever printing an empty field
# (the old awk-miss path produced lines like "Temperature: °C").
format_attr() {
    local value="$1" suffix="${2:-}"

    case "$value" in
        SMARTCTL_FAILED) echo "unavailable (smartctl could not read this device)" ;;
        NOT_PRESENT)     echo "unavailable (not reported by this device)" ;;
        *)               echo "${value}${suffix}" ;;
    esac
}

detect_das_drives() {
    log_header "Detecting DAS Drives"

    # Find all USB-attached SCSI disks behind the DAS enclosure.
    # Note: The enclosure presents its own model to sysfs, not the
    # individual drive model. Specific drives are verified by
    # serial number after detection.
    for dev in /sys/block/sd*; do
        local name
        name=$(basename "$dev")

        # Check if USB transport
        if [[ -L "$dev/device" ]]; then
            local transport
            transport=$(readlink -f "$dev/device" | grep -o "usb" || true)

            if [[ -n "$transport" ]]; then
                # Filter for the DAS enclosure by [das].model_pattern, a
                # substring of the disk's model; empty is no filter (every USB
                # disk), never "match nothing".
                local model
                model=$(cat "$dev/device/model" 2>/dev/null | tr -d ' ' || true)
                # Blanks are removed from the pattern as from the model
                # (docs/INSTALL.md; bd DAS-Backup-Manager-2kb1.15).
                local pattern="${DAS_MODEL_PATTERN:-}"
                pattern="${pattern//[[:space:]]/}"

                if [[ -z "$pattern" || "$model" == *"$pattern"* ]]; then
                    DAS_DEVICES+=("/dev/$name")
                fi
            fi
        fi
    done

    if [[ ${#DAS_DEVICES[@]} -eq 0 ]]; then
        log_error "No DAS drives detected!"
        log_error "Is the DAS enclosure connected and powered on?"
        exit 1
    fi

    echo "Found ${#DAS_DEVICES[@]} DAS drive(s):"
    for dev in "${DAS_DEVICES[@]}"; do
        local serial
        serial=$(read_drive_serial "$dev")
        local role="${DRIVE_MAP[$serial]:-Unknown}"
        echo "  $dev → Serial: $serial → $role"
    done
}

# parse_drive_serial <smartctl -i output> <its exit status>
# Returns 0 only when the output carries exactly one drive serial: one line
# that starts "Serial Number:" (ATA, NVMe) or "Serial number:" (SCSI, SAS),
# smartctl 7.5's own labels, with a non-empty value. Sets DRIVE_SERIAL to it
# and, on a refusal, DRIVE_SERIAL_PROBLEM to why. DRIVE_SERIALS_SEEN holds
# every non-empty value found either way, so a refusal can tell whether a
# listed drive was among them (bd DAS-Backup-Manager-tjph).
#
# A faithful copy of parse_drive_serial in
# scripts/das-wipe-all-drives-and-partition-from-scratch.sh (the scripts share
# no sourced library, so each ships its own; keep them in step, bd
# DAS-Backup-Manager-2kb1.17). Here it decides whether btrbk may write to a
# target found by serial: anything unreadable or ambiguous is a refusal.
#
# The identity rule: the whole value, with the blanks smartctl pads it with
# trimmed from both ends, compared byte for byte and case-sensitively. Before
# this rule the match was awk's third field of "Serial Number:" only: a SAS
# drive's "Serial number:" never matched, a value with a blank matched on its
# first word alone, and a smartctl that could not open the drive was believed.
parse_drive_serial() {
    local out="$1" rc="$2" nl=$'\n' rest line value count=0
    DRIVE_SERIAL="" DRIVE_SERIAL_PROBLEM="" DRIVE_SERIALS_SEEN=()

    # Line by line in bash itself: no pipe, no file, no subshell.
    rest="$out"
    while [[ -n $rest ]]; do
        line="${rest%%"$nl"*}"
        if [[ $rest == *"$nl"* ]]; then rest="${rest#*"$nl"}"; else rest=""; fi
        [[ $line == "Serial Number:"* || $line == "Serial number:"* ]] || continue
        value="${line#Serial [Nn]umber:}"
        value="${value#"${value%%[![:space:]]*}"}"
        value="${value%"${value##*[![:space:]]}"}"
        count=$((count + 1))
        [[ -z $value ]] || DRIVE_SERIALS_SEEN+=("$value")
    done

    # smartctl's exit status is a bitmask (smartctl(8), EXIT STATUS). Bits
    # 0-1: it could not parse its command line or open the drive, so nothing
    # it printed is the drive's identity. 127 (no smartctl) sets both. Other
    # bits are SMART findings, which leave the identity readable; the SMART
    # gate judges those.
    if ((rc & 3)); then
        DRIVE_SERIAL_PROBLEM="smartctl could not read its identity (exit $rc)"
        return 1
    fi
    if ((count > 1)); then
        DRIVE_SERIAL_PROBLEM="more than one serial line in smartctl's output (${DRIVE_SERIALS_SEEN[*]})"
        return 1
    fi
    if ((${#DRIVE_SERIALS_SEEN[@]} != 1)); then
        DRIVE_SERIAL_PROBLEM="no serial in smartctl's output"
        return 1
    fi
    DRIVE_SERIAL="${DRIVE_SERIALS_SEEN[0]}"
    return 0
}

# Read one drive's serial, or the empty string.
#
# The previous shape was:
#   serial=$(smartctl -i "$dev" 2>/dev/null | awk '.../{print $3; exit}' || echo "unknown")
#
# `awk ... exit` closes the pipe, smartctl dies of SIGPIPE (141), and under this
# script's `set -o pipefail` that fails the whole pipeline -- so `|| echo
# "unknown"` fired and APPENDED a second line AFTER awk had already printed the
# real serial. Measured on this host: serial=[SERIAL02\nunknown].
#
# Strict since 3.3.0 (bd DAS-Backup-Manager-2kb1.17): parse_drive_serial's rule,
# so a SAS "Serial number:" is read, a value is whole, and an unreadable or
# ambiguous serial is the empty string (never a guess).
#
# Two live consequences, both of which had been true for the entire life of the
# script: every drive rendered as "Unknown" instead of its configured display
# name, and in check_btrbk_status the `[[ "$serial" == "$primary_serial" ]]`
# comparison could never match -- so `primary_dev` was always empty and the
# whole btrbk/disk-usage section silently took its "primary not found" branch
# and had NEVER executed. bd DAS-Backup-Manager-nsp.
#
# Capture first, filter second: nothing can SIGPIPE a producer that has already
# finished, so the pipeline status means what it appears to mean.
read_drive_serial() {
    local dev="$1" out rc=0
    # shellcheck disable=SC2034 # DRIVE_SERIAL_PROBLEM is set by parse_drive_serial, unused here
    local DRIVE_SERIAL DRIVE_SERIAL_PROBLEM DRIVE_SERIALS_SEEN
    out="$(smartctl -i "$dev" 2>&1)" || rc=$?
    if parse_drive_serial "$out" "$rc"; then
        printf '%s' "$DRIVE_SERIAL"
    fi
}

check_smart_health() {
    log_header "SMART Health Check"

    local all_passed=true

    for dev in "${DAS_DEVICES[@]}"; do
        local serial
        serial=$(read_drive_serial "$dev")
        local role="${DRIVE_MAP[$serial]:-Unknown}"

        echo ""
        echo -e "${BLUE}── $dev ($role) ──${NC}"

        # What the RESULT block says of this drive: the worst thing found.
        local drive_status="OK" drive_why=""

        # Get SMART health
        local health
        health=$(smartctl -H "$dev" 2>/dev/null | grep -E "SMART overall-health" || echo "UNKNOWN")

        # Matched by bash itself — no pipe, no file, no fd. A pipe into
        # grep -q read a producer killed by SIGPIPE as "no match" under
        # pipefail (bd DAS-Backup-Manager-wkvz), and a here-string reads a
        # pipe or temp file it cannot make as "no match" too.
        if [[ $health == *PASSED* ]]; then
            echo -e "  Health: ${GREEN}PASSED${NC}"
        else
            echo -e "  Health: ${RED}$health${NC}"
            all_passed=false
            drive_status="FAIL"
            drive_why="SMART health not PASSED (${health##*: })"
        fi

        # Check for pending/reallocated sectors. "attribute missing" and
        # "smartctl failed" are distinct states from a real zero and must never
        # be collapsed into one.
        local reallocated pending
        reallocated=$(smart_attr_raw "$dev" "Reallocated_Sector")
        pending=$(smart_attr_raw "$dev" "Current_Pending_Sector")

        if ! report_sector_attr "Reallocated Sectors" "$reallocated"; then
            all_passed=false
            drive_status="FAIL"
            drive_why="${drive_why:+$drive_why; }reallocated sectors could not be read"
        elif [[ $reallocated != 0 && $drive_status == "OK" ]]; then
            drive_status="WARN"
            drive_why="$reallocated reallocated sectors"
        fi
        if ! report_sector_attr "Pending Sectors" "$pending"; then
            all_passed=false
            drive_status="FAIL"
            drive_why="${drive_why:+$drive_why; }pending sectors could not be read"
        elif [[ $pending != 0 && $drive_status == "OK" ]]; then
            drive_status="WARN"
            drive_why="$pending pending sectors"
        fi

        # Check power-on hours and temperature
        local hours temp
        hours=$(smart_attr_raw "$dev" "Power_On_Hours")
        temp=$(smart_attr_raw "$dev" "Temperature_Celsius")

        echo "  Power-On Hours: $(format_attr "$hours")"
        echo "  Temperature: $(format_attr "$temp" "°C")"

        # Check for running/completed self-tests. The log is captured whole
        # first: piping smartctl straight into `head -1` closed the pipe while
        # smartctl was still writing, killing it mid-read.
        local selftest_log test_status
        selftest_log=$(smartctl -l selftest "$dev" 2>/dev/null) || true
        test_status=$(printf '%s\n' "$selftest_log" | grep -E "^# *1[[:space:]]" | head -1 || true)
        echo "  Last Test: ${test_status:-unavailable (no self-test log entry)}"
        DRIVE_RESULTS+=("$dev ($role)|$drive_status|${drive_why:-SMART health PASSED}")
    done

    echo ""
    if $all_passed; then
        log_info "All drives passed SMART health check"
    else
        SMART_FAILED=true
        log_warn "One or more drives have SMART issues - investigate!"
    fi
}

# read_holder_record <lock file>: how the record in a lock file reads, for the
# scripts that name a lock's holder. Sets HOLDER_LINE (its first line, without
# control characters, at most 200 characters, trimmed) and HOLDER_STATE:
#   unreadable  the file cannot be read          empty    nothing recorded
#   nopid       no `<who> pid <digits>` shape    gone     that pid no longer runs
#   running     that pid runs
# A faithful copy sits in scripts/backup-run.sh and scripts/backup-verify.sh
# (installed scripts source nothing: each is self-contained where it runs);
# tests/test_maintenance_lock.sh fails if the two differ (bd
# DAS-Backup-Manager-2kb1.18). The words each script shows for a state stay in
# its own wrapper. The reading is the indexer's maintenance::holder_of.
read_holder_record() {
    local lock="$1" line="" pid
    HOLDER_LINE="" HOLDER_STATE=""
    if [[ ! -r "$lock" ]]; then
        HOLDER_STATE=unreadable
        return 0
    fi
    IFS= read -r line <"$lock" || true # a last line without a newline still fills $line
    line="${line//[[:cntrl:]]/}"
    line="${line:0:200}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    HOLDER_LINE="$line"
    if [[ -z "$line" ]]; then
        HOLDER_STATE=empty
        return 0
    fi
    pid="${line##* pid }"
    if [[ "$line" != *" pid "* || ! "$pid" =~ ^[[:digit:]]+$ ]]; then
        HOLDER_STATE=nopid
    elif [[ -d "/proc/$pid" ]]; then
        HOLDER_STATE=running
    else
        HOLDER_STATE=gone
    fi
    return 0
}

# Who holds the lock, as the holder recorded itself in the lock file: its first
# line, `<who> pid <pid>`. Said plainly when there is nothing to name: an empty
# or unreadable record, a record without a pid (shown as the last one recorded),
# or a pid that no longer runs (a holder that did not record itself, such as a
# plain `flock`, holds it after one that did). The same reading as
# backup-run.sh's maintenance_holder() and the indexer's maintenance::holder_of,
# in the words of a line that is only displayed (bd DAS-Backup-Manager-3wc).
maintenance_lock_holder() {
    local lock="$1"
    read_holder_record "$lock"
    case "$HOLDER_STATE" in
        unreadable) echo "an unknown holder (its record in $lock cannot be read)" ;;
        empty) echo "an unknown holder (its record in $lock is empty)" ;;
        nopid) echo "an unknown holder (last recorded: $HOLDER_LINE)" ;;
        running) echo "$HOLDER_LINE" ;;
        *) echo "an unknown holder (the last recorded holder, $HOLDER_LINE, is no longer running)" ;;
    esac
}

# Take the DAS maintenance lock without waiting, on fd 8 (held until this
# script exits), and record this script as its holder — in the lock file, as
# every holder does, so a job that finds the lock held can say what it waits
# for. The EXIT trap empties the record again while fd 8 still holds the
# lock. Returns non-zero, having said why, when the lock cannot be opened or
# is held. Opened `<>`, not `>`: `>` would empty the current holder's record
# (bd DAS-Backup-Manager-frb).
take_maintenance_lock() {
    local lock="$1"
    if ! exec 8<>"$lock"; then
        log_warn "Cannot open $lock — skipping btrbk/usage inspection"
        return 1
    fi
    # Root's alone: a lock file anyone can open is one anyone can hold (bd qab).
    if [[ -f "$lock" && ! -L "$lock" ]] && ! chmod 600 -- "$lock"; then
        log_warn "Cannot make $lock root's alone (0600): any local user can hold it"
    fi
    if ! flock -n 8; then
        # Not "backup or scrub": every job that mounts the targets holds it.
        log_warn "DAS maintenance lock held by $(maintenance_lock_holder "$lock") — skipping btrbk/usage inspection"
        log_warn "  Re-run when it finishes; this section mounts the array read-only."
        return 1
    fi
    MAINTENANCE_RECORD="$lock"
    trap clear_maintenance_record EXIT
    if ! printf 'backup-verify.sh pid %s\n' "$$" >"$lock"; then
        log_warn "Could not record backup-verify.sh as the holder of $lock"
    fi
}

# Empty this script's holder record, as the EXIT trap, while fd 8 still holds
# the lock — so a finished run is never named as the holder. Installed only
# once the lock is held; before that the record is another holder's. Emptied,
# never removed: the file is the lock. Display only: a failure is logged.
# Run only through the EXIT trap. shellcheck 0.11 stops seeing a trap's
# handlers as called once the last line ends in `exit` (SC2329).
# shellcheck disable=SC2329
clear_maintenance_record() {
    if ! : >"$MAINTENANCE_RECORD"; then
        log_warn "Could not empty the holder record in $MAINTENANCE_RECORD"
    fi
}

# Mount a RAID-1 primary through the guard, read-only intent, handed this
# script's maintenance lock (fd 8, from take_maintenance_lock): the one rule the
# Rust jobs and backup-run.sh mount with. It prints one line, `RAID GUARD:
# <STATUS>  (<detail>)`, and exits 0 mounted (any status), 1 not mounted, 3
# refused, 2 could not run. The exit status travels inside the capture, never
# as an `if x="$(cmd)"` test (fail-silent.md); anything but a captured 0 is a
# failure, its reason left in the global GUARD_MOUNT_ERR (set here, never at the
# top level of the file: a rewrite of the script must not land mid-command).
mount_primary_guarded() { # mount_primary_guarded <label>
    local label="$1" out last rc line
    GUARD_MOUNT_ERR=""
    out="$(DAS_MAINTENANCE_LOCK_FD=8 "${BTRDASD_BIN:-/usr/bin/btrdasd}" backup mount-target \
        --label "$label" --config "${DAS_CONFIG:-/etc/das-backup/config.toml}" --read-only &&
        printf '\nstatus=0' || printf '\nstatus=%s' "$?")"
    last="${out##*$'\n'}"
    if [[ "$last" == status=* ]]; then
        rc="${last#status=}"
    else
        rc="uncaptured"
    fi
    line="$(grep -m1 '^RAID GUARD: ' <<<"$out" || true)"
    if [[ "$rc" == 0 ]]; then
        # Any status is a mount; a WARN-class one is said, not hidden.
        if [[ -n "$line" && "$line" != "RAID GUARD: OK "* ]]; then
            log_warn "  $label: ${line#RAID GUARD: }"
        fi
        return 0
    fi
    GUARD_MOUNT_ERR="(exit $rc): ${line:-no RAID GUARD line}"
    return 1
}

check_btrbk_status() {
    log_header "btrbk Backup Status"

    if [[ ! -f "$DAS_BTRBK_CONF" ]]; then
        log_warn "btrbk not configured ($DAS_BTRBK_CONF missing)"
        BTRBK_RESULT="WARN|btrbk not configured"
        return
    fi

    # Find primary backup drive by serial number from config
    local primary_serial=""
    local primary_mount=""
    local primary_label=""
    local primary_guarded=false
    for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
        local role_var="DAS_TARGET_${i}_ROLE"
        if [[ "${!role_var}" == "primary" ]]; then
            local serial_var="DAS_TARGET_${i}_SERIAL"
            local mount_var="DAS_TARGET_${i}_MOUNT"
            local label_var="DAS_TARGET_${i}_LABEL"
            local uuid_var="DAS_TARGET_${i}_MOUNT_UUID"
            local serials_var="DAS_TARGET_${i}_SERIALS"
            local -a member_serials
            primary_serial="${!serial_var}"
            primary_mount="${!mount_var}"
            primary_label="${!label_var:-}"
            # raid_guard::is_guarded: a filesystem UUID and more than one
            # member serial. Such a target is mounted only by the guard.
            read -ra member_serials <<<"${!serials_var:-}"
            if [[ -n "${!uuid_var:-}" && ${#member_serials[@]} -gt 1 ]]; then
                primary_guarded=true
            fi
            break
        fi
    done

    local primary_dev=""
    for dev in "${DAS_DEVICES[@]}"; do
        local serial
        serial=$(read_drive_serial "$dev")
        if [[ -n "$serial" && "$serial" == "$primary_serial" ]]; then
            primary_dev="${dev}1"  # Single partition, whole-disk BTRFS
        fi
    done

    # Check if primary backup drive is mountable. The mount error is captured:
    # discarding it made every failure - busy, wrong filesystem, or a RAID-1 leg
    # missing and needing 'degraded' - read as "not found or not formatted".
    local mounted=false
    local mount_err=""

    # This branch was unreachable until the serial fix above, so it is
    # effectively new code and must obey the rules the rest of the project
    # already follows.
    #
    # (a) MAINTENANCE INTERLOCK. Backups and the scrub engine both mount and
    #     unmount these same filesystems and serialise behind
    #     /run/das-maintenance.lock. This script took no lock at all, so
    #     mounting here could collide with a running backup. Non-blocking on
    #     purpose: backup-verify is an interactive diagnostic, and waiting up
    #     to an hour behind a backup is not what an operator asked for --
    #     skipping with a clear reason is.
    # (b) THE RAID-1 GUARD. A RAID-1 primary (raid_guard::is_guarded) is
    #     mounted only by the guard, never by a hand `mount` (bd
    #     DAS-Backup-Manager-su1b.10, su1b.23): `btrdasd backup mount-target
    #     --read-only`, handed the lock taken here as fd 8. The guard mounts
    #     without `degraded`, mounts degraded only on a member truly absent,
    #     and refuses a rejected or split-brain member; a hand mount with
    #     `degraded` can read a stale member's tree and skips those checks.
    #     --read-only is the guard's intent for a job that persists nothing
    #     derived from what it reads: this script only lists, reads usage
    #     and unmounts. A single-device primary keeps the plain read-only
    #     mount, without `degraded`.
    #
    # A held lock is a deliberate skip, which take_maintenance_lock has already
    # explained — not a failed mount (bd DAS-Backup-Manager-0oi).
    local maint_lock="/run/das-maintenance.lock"
    local skipped=false
    local attempted=false
    # A guarded target's members are found by the guard (by filesystem UUID),
    # so a missing anchor drive is for the guard to judge, not a reason to
    # skip it here.
    if { $primary_guarded && [[ -n "$primary_label" ]]; } ||
        [[ -n "$primary_dev" && -b "$primary_dev" ]]; then
        if take_maintenance_lock "$maint_lock"; then
            attempted=true
            mkdir -p "$primary_mount"
            if $primary_guarded && [[ -n "$primary_label" ]]; then
                if mount_primary_guarded "$primary_label"; then
                    mounted=true
                else
                    mount_err="$GUARD_MOUNT_ERR"
                fi
            else
                local mount_stderr
                mount_stderr=$(mktemp)
                if mount -o ro,nossd,noatime "$primary_dev" "$primary_mount" 2>"$mount_stderr"; then
                    mounted=true
                else
                    mount_err=$(tr '\n' ' ' < "$mount_stderr")
                fi
                [[ -z "${mount_stderr:-}" ]] || rm -f "${mount_stderr:?}"
            fi
        else
            skipped=true
        fi
    fi

    if $mounted; then
        echo ""
        echo "Latest snapshots:"
        # "btrbk failed" and "btrbk ran and found nothing" are different facts;
        # `|| echo "(no snapshots yet)"` reported the first as the second.
        local btrbk_out btrbk_stderr btrbk_failed=false
        btrbk_stderr=$(mktemp)
        # Scoped to the primary's target directories (a quoted glob, read by
        # btrbk): only the primary is mounted here, and an unscoped listing
        # fails on every unmounted recovery target, so the section said
        # "snapshot status UNKNOWN" on every run (bd DAS-Backup-Manager-su1b.23).
        if btrbk_out=$(btrbk -c "$DAS_BTRBK_CONF" list latest "$primary_mount/*" 2>"$btrbk_stderr"); then
            if [[ -n "$btrbk_out" ]]; then
                printf '%s\n' "$btrbk_out"
            else
                echo "  (no snapshots yet)"
            fi
        else
            log_error "  btrbk list failed - snapshot status UNKNOWN: $(tr '\n' ' ' < "$btrbk_stderr")"
            btrbk_failed=true
        fi
        [[ -z "${btrbk_stderr:-}" ]] || rm -f "${btrbk_stderr:?}"

        echo ""
        echo "Disk usage:"
        df -h "$primary_mount"

        echo ""
        echo "BTRFS usage:"
        btrfs filesystem usage "$primary_mount" 2>/dev/null | head -8

        # Cleanup
        if $btrbk_failed; then
            BTRBK_RESULT="FAIL|btrbk list failed, snapshot status unknown"
        else
            BTRBK_RESULT="OK|primary mounted read-only, snapshots and usage read"
        fi
        umount "$primary_mount" 2>/dev/null || {
            log_warn "Failed to unmount $primary_mount"
            BTRBK_RESULT="WARN|primary read, but $primary_mount was not unmounted"
        }
    elif $skipped; then
        BTRBK_RESULT="SKIPPED|the DAS maintenance lock is held or could not be opened"
    elif $attempted; then
        BTRBK_RESULT="FAIL|the primary could not be mounted"
        if $primary_guarded && [[ -n "$primary_label" ]]; then
            log_error "Failed to mount $primary_label at $primary_mount through the RAID-1 guard ${mount_err:-(no error output)}"
        else
            log_error "Failed to mount $primary_dev at $primary_mount: ${mount_err:-(no error output)}"
        fi
    elif [[ -z "$primary_dev" ]]; then
        log_warn "Primary backup drive (serial ${primary_serial:-unset}) not among the detected DAS devices"
        BTRBK_RESULT="WARN|the primary drive is not among the detected DAS devices"
    elif [[ ! -b "$primary_dev" ]]; then
        log_warn "Primary backup partition $primary_dev is not a block device"
        BTRBK_RESULT="WARN|the primary partition is not a block device"
    fi
}

show_summary() {
    log_header "Summary"

    echo "DAS Drives Detected: ${#DAS_DEVICES[@]}"
    echo ""
    echo -e "${BLUE}── RESULT ──${NC}"
    local row
    for row in "${DRIVE_RESULTS[@]}"; do
        local rest="${row#*|}"
        result_row "${row%%|*}" "${rest%%|*}" "${rest#*|}"
    done
    result_row "btrbk backup status" "${BTRBK_RESULT%%|*}" "${BTRBK_RESULT#*|}"
    echo ""
    echo "Next steps:"
    echo "  1. If SMART tests are still running, wait for completion"
    echo "  2. Check test results: smartctl -l selftest /dev/sdX"
    echo "  3. Run backup: sudo ./backup-run.sh"
}

# ============================================================================
# MAIN
# ============================================================================

main() {
    local quick_mode=false

    if [[ "${1:-}" == "--quick" ]] || [[ "${1:-}" == "-q" ]]; then
        quick_mode=true
        BTRBK_RESULT="SKIPPED|--quick: the backup-status check was not run"
    fi

    echo -e "${BLUE}━━ DAS Backup Verification ━━${NC}"
    echo "Date: $(date '+%Y-%m-%d %H:%M:%S %Z')"

    load_configuration
    check_root
    detect_das_drives
    check_smart_health

    if ! $quick_mode; then
        check_btrbk_status
    fi

    show_summary

    # Exit status must reflect the SMART result. Ending on show_summary made the
    # script exit 0 even with a failing drive, so no caller or timer could gate
    # on it.
    if $SMART_FAILED; then
        echo ""
        log_error "Verification FAILED: SMART health could not be confirmed on one or more drives"
        return 1
    fi

    return 0
}

main "$@"; exit $?
