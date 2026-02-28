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
BTRDASD_BIN="${BTRDASD_BIN:-/usr/local/bin/btrdasd}"
DAS_CONFIG="${DAS_CONFIG:-/etc/das-backup/config.toml}"
if [[ -x "$BTRDASD_BIN" ]]; then
    eval "$("$BTRDASD_BIN" config dump-env --config "$DAS_CONFIG")"
else
    echo "ERROR: btrdasd not found at $BTRDASD_BIN" >&2
    exit 1
fi

# Build drive map from config targets (serial -> display name)
declare -A DRIVE_MAP=()
declare -A TARGET_ROLES=()
for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
    serial_var="DAS_TARGET_${i}_SERIAL"
    name_var="DAS_TARGET_${i}_DISPLAY_NAME"
    label_var="DAS_TARGET_${i}_LABEL"
    role_var="DAS_TARGET_${i}_ROLE"
    mount_var="DAS_TARGET_${i}_MOUNT"
    serial="${!serial_var}"
    if [[ -n "${!name_var:-}" ]]; then
        DRIVE_MAP[$serial]="${!name_var}"
    else
        DRIVE_MAP[$serial]="${!label_var}"
    fi
    TARGET_ROLES[$serial]="${!role_var}"
done

# Expected DAS drives (detected by USB transport)
DAS_DEVICES=()

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
        serial=$(smartctl -i "$dev" 2>/dev/null | awk '/Serial Number:/{print $3; exit}' || echo "unknown")
        local role="${DRIVE_MAP[$serial]:-Unknown}"
        echo "  $dev → Serial: $serial → $role"
    done
}

check_smart_health() {
    log_header "SMART Health Check"

    local all_passed=true

    for dev in "${DAS_DEVICES[@]}"; do
        local serial
        serial=$(smartctl -i "$dev" 2>/dev/null | awk '/Serial Number:/{print $3; exit}' || echo "unknown")
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

        # Check for pending/reallocated sectors
        local reallocated pending
        reallocated=$(smartctl -A "$dev" 2>/dev/null | grep "Reallocated_Sector" | awk '{print $10}' || echo "0")
        pending=$(smartctl -A "$dev" 2>/dev/null | grep "Current_Pending_Sector" | awk '{print $10}' || echo "0")

        if [[ "$reallocated" != "0" ]]; then
            echo -e "  Reallocated Sectors: ${YELLOW}$reallocated${NC}"
        else
            echo -e "  Reallocated Sectors: ${GREEN}0${NC}"
        fi

        if [[ "$pending" != "0" ]]; then
            echo -e "  Pending Sectors: ${YELLOW}$pending${NC}"
        else
            echo -e "  Pending Sectors: ${GREEN}0${NC}"
        fi

        # Check power-on hours and temperature
        local hours temp
        hours=$(smartctl -A "$dev" 2>/dev/null | grep "Power_On_Hours" | awk '{print $10}' || echo "unknown")
        temp=$(smartctl -A "$dev" 2>/dev/null | grep "Temperature_Celsius" | awk '{print $10}' || echo "unknown")

        echo "  Power-On Hours: $hours"
        echo "  Temperature: ${temp}°C"

        # Check for running/completed self-tests
        local test_status
        test_status=$(smartctl -l selftest "$dev" 2>/dev/null | grep -E "# 1" | head -1 || echo "No tests")
        echo "  Last Test: $test_status"
    done

    echo ""
    if $all_passed; then
        log_info "All drives passed SMART health check"
    else
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
        serial=$(smartctl -i "$dev" 2>/dev/null | awk '/Serial Number:/{print $3; exit}' || echo "unknown")
        if [[ "$serial" == "$primary_serial" ]]; then
            primary_dev="${dev}1"  # Single partition, whole-disk BTRFS
        fi
    done

    # Check if primary backup drive is mountable
    local mounted=false

    if [[ -n "$primary_dev" && -b "$primary_dev" ]]; then
        mkdir -p "$primary_mount"
        if mount -o ro,nossd,noatime "$primary_dev" "$primary_mount" 2>/dev/null; then
            mounted=true
        fi
    fi

    if $mounted; then
        echo ""
        echo "Latest snapshots:"
        btrbk -c "$DAS_BTRBK_CONF" list latest 2>/dev/null || echo "  (no snapshots yet)"

        echo ""
        echo "Disk usage:"
        df -h "$primary_mount"

        echo ""
        echo "BTRFS usage:"
        btrfs filesystem usage "$primary_mount" 2>/dev/null | head -8

        # Cleanup
        umount "$primary_mount" 2>/dev/null || log_warn "Failed to unmount $primary_mount"
    else
        log_warn "Primary backup drive not found or not formatted"
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
}

main "$@"
