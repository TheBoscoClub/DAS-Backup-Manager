#!/bin/bash
# backup-verify.sh - Verify DAS drive health and backup status (config-driven)
# Version: 3.0.0
# Date: 2026-02-21
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

# Load configuration from config.toml via btrdasd
BTRDASD_BIN="${BTRDASD_BIN:-/usr/bin/btrdasd}"
DAS_CONFIG="${DAS_CONFIG:-/etc/das-backup/config.toml}"
if [[ -x "$BTRDASD_BIN" ]]; then
    eval "$("$BTRDASD_BIN" config dump-env --config "$DAS_CONFIG")"
else
    echo "ERROR: btrdasd not found at $BTRDASD_BIN" >&2
    exit 1
fi

# Build drive map from config targets (serial -> display name)
declare -A DRIVE_MAP=()
for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
    serial_var="DAS_TARGET_${i}_SERIAL"
    name_var="DAS_TARGET_${i}_DISPLAY_NAME"
    label_var="DAS_TARGET_${i}_LABEL"
    mount_var="DAS_TARGET_${i}_MOUNT"
    serial="${!serial_var}"
    if [[ -n "${!name_var:-}" ]]; then
        DRIVE_MAP[$serial]="${!name_var}"
    else
        DRIVE_MAP[$serial]="${!label_var}"
    fi
done

# Expected DAS drives (detected by USB transport)
DAS_DEVICES=()

# Set by check_smart_health when any drive fails or cannot be read. main()
# exits nonzero on it so this script can be used as a gate.
SMART_FAILED=false

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

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
    echo -e "${BLUE}========================================${NC}"
    echo -e "${BLUE}  $1${NC}"
    echo -e "${BLUE}========================================${NC}"
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
            if [[ "$value" =~ ^[0-9]+$ ]]; then
                echo -e "  $label: ${YELLOW}$value${NC}"
                return 0
            fi
            echo -e "  $label: ${RED}UNKNOWN (unparseable value: $value)${NC}"
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
                # Filter for DAS enclosure by model pattern from config
                local model
                model=$(cat "$dev/device/model" 2>/dev/null | tr -d ' ' || true)

                if [[ "$model" == "$DAS_MODEL_PATTERN" ]]; then
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

# Read one drive's serial, or the empty string.
#
# The previous shape was:
#   serial=$(smartctl -i "$dev" 2>/dev/null | awk '.../{print $3; exit}' || echo "unknown")
#
# `awk ... exit` closes the pipe, smartctl dies of SIGPIPE (141), and under this
# script's `set -o pipefail` that fails the whole pipeline -- so `|| echo
# "unknown"` fired and APPENDED a second line AFTER awk had already printed the
# real serial. Measured on this host: serial=[ZFL41DNY\nunknown].
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
    local dev="$1" info
    info=$(smartctl -i "$dev" 2>/dev/null) || true
    printf '%s' "$(awk '/Serial Number:/{print $3; exit}' <<<"$info")"
}

check_smart_health() {
    log_header "SMART Health Check"

    local all_passed=true

    for dev in "${DAS_DEVICES[@]}"; do
        local serial
        serial=$(read_drive_serial "$dev")
        local role="${DRIVE_MAP[$serial]:-Unknown}"

        echo ""
        echo -e "${BLUE}--- $dev ($role) ---${NC}"

        # Get SMART health
        local health
        health=$(smartctl -H "$dev" 2>/dev/null | grep -E "SMART overall-health" || echo "UNKNOWN")

        if echo "$health" | grep -q "PASSED"; then
            echo -e "  Health: ${GREEN}PASSED${NC}"
        else
            echo -e "  Health: ${RED}$health${NC}"
            all_passed=false
        fi

        # Check for pending/reallocated sectors. "attribute missing" and
        # "smartctl failed" are distinct states from a real zero and must never
        # be collapsed into one.
        local reallocated pending
        reallocated=$(smart_attr_raw "$dev" "Reallocated_Sector")
        pending=$(smart_attr_raw "$dev" "Current_Pending_Sector")

        report_sector_attr "Reallocated Sectors" "$reallocated" || all_passed=false
        report_sector_attr "Pending Sectors" "$pending" || all_passed=false

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
    done

    echo ""
    if $all_passed; then
        log_info "All drives passed SMART health check"
    else
        SMART_FAILED=true
        log_warn "One or more drives have SMART issues - investigate!"
    fi
}

check_btrbk_status() {
    log_header "btrbk Backup Status"

    if [[ ! -f "$DAS_BTRBK_CONF" ]]; then
        log_warn "btrbk not configured ($DAS_BTRBK_CONF missing)"
        return
    fi

    # Find primary backup drive by serial number from config
    local primary_serial=""
    local primary_mount=""
    for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
        local role_var="DAS_TARGET_${i}_ROLE"
        if [[ "${!role_var}" == "primary" ]]; then
            local serial_var="DAS_TARGET_${i}_SERIAL"
            local mount_var="DAS_TARGET_${i}_MOUNT"
            primary_serial="${!serial_var}"
            primary_mount="${!mount_var}"
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
    # (b) `degraded`. The 22 TB primary is BTRFS RAID-1; per
    #     .claude/rules/backup.md its mount options carry `degraded` so a
    #     single-leg failure does not block inspection. A verify tool that
    #     cannot look at a degraded array is useless exactly when it matters.
    local maint_lock="/run/das-maintenance.lock"
    if [[ -n "$primary_dev" && -b "$primary_dev" ]] && ! exec 8>"$maint_lock"; then
        log_warn "Cannot open $maint_lock — skipping btrbk/usage inspection"
    elif [[ -n "$primary_dev" && -b "$primary_dev" ]] && ! flock -n 8; then
        log_warn "DAS maintenance lock held (backup or scrub running) — skipping btrbk/usage inspection"
        log_warn "  Re-run when the backup finishes; this section mounts the array read-only."
    elif [[ -n "$primary_dev" && -b "$primary_dev" ]]; then
        mkdir -p "$primary_mount"
        local mount_stderr
        mount_stderr=$(mktemp)
        if mount -o ro,nossd,noatime,degraded "$primary_dev" "$primary_mount" 2>"$mount_stderr"; then
            mounted=true
        else
            mount_err=$(tr '\n' ' ' < "$mount_stderr")
        fi
        rm -f "$mount_stderr"
    fi

    if $mounted; then
        echo ""
        echo "Latest snapshots:"
        # "btrbk failed" and "btrbk ran and found nothing" are different facts;
        # `|| echo "(no snapshots yet)"` reported the first as the second.
        local btrbk_out btrbk_stderr
        btrbk_stderr=$(mktemp)
        if btrbk_out=$(btrbk -c "$DAS_BTRBK_CONF" list latest 2>"$btrbk_stderr"); then
            if [[ -n "$btrbk_out" ]]; then
                printf '%s\n' "$btrbk_out"
            else
                echo "  (no snapshots yet)"
            fi
        else
            log_error "  btrbk list failed - snapshot status UNKNOWN: $(tr '\n' ' ' < "$btrbk_stderr")"
        fi
        rm -f "$btrbk_stderr"

        echo ""
        echo "Disk usage:"
        df -h "$primary_mount"

        echo ""
        echo "BTRFS usage:"
        btrfs filesystem usage "$primary_mount" 2>/dev/null | head -8

        # Cleanup
        umount "$primary_mount" 2>/dev/null || log_warn "Failed to unmount $primary_mount"
    elif [[ -z "$primary_dev" ]]; then
        log_warn "Primary backup drive (serial ${primary_serial:-unset}) not among the detected DAS devices"
    elif [[ ! -b "$primary_dev" ]]; then
        log_warn "Primary backup partition $primary_dev is not a block device"
    else
        log_error "Failed to mount $primary_dev at $primary_mount: ${mount_err:-(no error output)}"
    fi
}

show_summary() {
    log_header "Summary"

    echo "DAS Drives Detected: ${#DAS_DEVICES[@]}"
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
    fi

    echo "========================================"
    echo "  DAS Backup Verification"
    echo "  Date: $(date '+%Y-%m-%d %H:%M:%S')"
    echo "========================================"

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

main "$@"
