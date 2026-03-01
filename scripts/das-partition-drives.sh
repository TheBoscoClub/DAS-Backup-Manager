#!/bin/bash
# das-partition-drives.sh - Partition and format DAS backup drives (config-driven)
# Version: 2.0.0
# Date: 2026-02-21
#
# WARNING: This script DESTROYS ALL DATA on the target drives!
#     Run ONLY after verifying SMART tests passed.
#     All configuration loaded from config.toml via btrdasd.
#
# Drive Layout (from config):
#   Bootable targets (role with ESP):
#     - Partition 1: 1.5G ESP (FAT32) - clone of /boot
#     - Partition 2: remainder BTRFS - system subvolumes
#
#   Primary/data targets (no ESP):
#     - Whole disk BTRFS
#
# Usage:
#   sudo ./das-partition-drives.sh --check   # Verify drives, show plan
#   sudo ./das-partition-drives.sh --run     # Execute partitioning

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

# Build device-to-serial mapping and target info from config
declare -A EXPECTED_SERIALS=()
declare -A TARGET_LABELS=()
declare -A TARGET_ROLES=()
declare -A TARGET_NAMES=()

for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
    label_var="DAS_TARGET_${i}_LABEL"
    serial_var="DAS_TARGET_${i}_SERIAL"
    role_var="DAS_TARGET_${i}_ROLE"
    name_var="DAS_TARGET_${i}_DISPLAY_NAME"
    serial="${!serial_var}"
    label="${!label_var}"
    EXPECTED_SERIALS[$serial]="$label"
    TARGET_LABELS[$serial]="$label"
    TARGET_ROLES[$serial]="${!role_var}"
    if [[ -n "${!name_var:-}" ]]; then
        TARGET_NAMES[$serial]="${!name_var}"
    else
        TARGET_NAMES[$serial]="$label"
    fi
done

# BTRFS label prefix
BTRFS_LABEL_PREFIX="das-backup"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# ============================================================================
# FUNCTIONS
# ============================================================================

log_info() { echo -e "${GREEN}[INFO]${NC} $1"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }
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

# Discover devices by serial number (returns associative array in DISCOVERED_DEVICES)
declare -A DISCOVERED_DEVICES=()

discover_devices() {
    log_header "Discovering DAS Drives by Serial Number"

    for dev in /dev/sd[a-z] /dev/sd[a-z][a-z]; do
        if [[ -b "$dev" ]]; then
            local serial
            serial=$(smartctl -i "$dev" 2>/dev/null | awk '/Serial Number:/{print $3}' || true)
            if [[ -n "$serial" && -n "${EXPECTED_SERIALS[$serial]:-}" ]]; then
                DISCOVERED_DEVICES[$serial]="$dev"
            fi
        fi
    done
}

verify_serials() {
    log_header "Verifying Drive Serial Numbers"

    local all_found=true

    for serial in "${!EXPECTED_SERIALS[@]}"; do
        local label="${EXPECTED_SERIALS[$serial]}"
        local dev="${DISCOVERED_DEVICES[$serial]:-}"

        if [[ -n "$dev" ]]; then
            echo -e "  $serial ($label): ${GREEN}$dev${NC}"
        else
            echo -e "  $serial ($label): ${RED}NOT FOUND${NC}"
            all_found=false
        fi
    done

    if ! $all_found; then
        log_error "Not all drives found! Check DAS connections."
        log_error "Run 'lsblk' and verify drive serials."
        exit 1
    fi

    log_info "All drive serials verified"
}

check_smart_tests() {
    log_header "Checking SMART Test Status"

    local all_complete=true

    for serial in "${!DISCOVERED_DEVICES[@]}"; do
        local dev="${DISCOVERED_DEVICES[$serial]}"
        local label="${TARGET_LABELS[$serial]}"
        local status
        status=$(smartctl -l selftest "$dev" 2>/dev/null | grep -E "# 1" | head -1 || echo "No tests")

        if echo "$status" | grep -qE "in progress|Self-test routine in progress"; then
            echo -e "  $dev ($label): ${YELLOW}Test still running${NC}"
            all_complete=false
        elif echo "$status" | grep -q "Completed without error"; then
            echo -e "  $dev ($label): ${GREEN}Test completed - PASSED${NC}"
        else
            echo -e "  $dev ($label): ${YELLOW}$status${NC}"
        fi
    done

    if ! $all_complete; then
        log_warn "SMART tests still running. Wait for completion before partitioning."
        return 1
    fi

    log_info "All SMART tests complete"
    return 0
}

show_plan() {
    log_header "Partitioning Plan"

    echo ""
    for serial in "${!DISCOVERED_DEVICES[@]}"; do
        local dev="${DISCOVERED_DEVICES[$serial]}"
        local label="${TARGET_LABELS[$serial]}"
        local role="${TARGET_ROLES[$serial]}"
        local name="${TARGET_NAMES[$serial]}"

        echo "$name ($dev, serial: $serial):"
        if [[ "$role" == "primary" ]]; then
            echo "  Whole disk BTRFS (single partition) - label: ${BTRFS_LABEL_PREFIX}-${label}"
        elif [[ "$role" == *"esp"* || "$role" == *"boot"* || "$role" == *"system"* || "$role" == *"mirror"* ]]; then
            echo "  Partition 1: 1.5G ESP (FAT32) - EFI System Partition"
            echo "  Partition 2: remainder BTRFS - label: ${BTRFS_LABEL_PREFIX}-${label}"
        else
            echo "  Whole disk BTRFS - label: ${BTRFS_LABEL_PREFIX}-${label}"
        fi
        echo ""
    done
}

confirm_destruction() {
    echo ""
    echo -e "${RED}╔══════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${RED}║  WARNING: ALL DATA ON TARGET DRIVES WILL BE DESTROYED!     ║${NC}"
    echo -e "${RED}╚══════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo "Drives to be wiped:"
    for serial in "${!DISCOVERED_DEVICES[@]}"; do
        local dev="${DISCOVERED_DEVICES[$serial]}"
        local name="${TARGET_NAMES[$serial]}"
        echo "  $dev — $name ($(lsblk -dn -o SIZE "$dev"))"
    done
    echo ""
    read -rp "Type 'YES-DESTROY' to proceed: " confirm

    if [[ "$confirm" != "YES-DESTROY" ]]; then
        log_info "Aborted by user"
        exit 0
    fi
}

partition_bootable_drive() {
    local dev="$1"
    local label="$2"

    log_info "Partitioning $dev (bootable)..."

    # Wipe existing partition table
    wipefs -a "$dev"

    # Create GPT partition table with ESP + BTRFS
    parted -s "$dev" mklabel gpt
    parted -s "$dev" mkpart ESP fat32 1MiB 1537MiB  # 1.5G ESP
    parted -s "$dev" set 1 esp on
    parted -s "$dev" mkpart primary btrfs 1537MiB 100%

    # Wait for kernel to update
    partprobe "$dev"
    sleep 2

    # Format ESP
    log_info "  Formatting ${dev}1 as FAT32 (ESP)..."
    mkfs.fat -F32 -n "BACKUP-ESP" "${dev}1"

    # Format BTRFS partition
    log_info "  Formatting ${dev}2 as BTRFS..."
    mkfs.btrfs -f -L "$label" "${dev}2"

    log_info "  $dev partitioned successfully"
}

partition_data_drive() {
    local dev="$1"
    local label="$2"

    log_info "Partitioning $dev (data)..."

    # Wipe existing partition table
    wipefs -a "$dev"

    # Create GPT with single BTRFS partition
    parted -s "$dev" mklabel gpt
    parted -s "$dev" mkpart primary btrfs 1MiB 100%

    # Wait for kernel to update
    partprobe "$dev"
    sleep 2

    # Format as BTRFS (whole disk minus GPT overhead)
    log_info "  Formatting ${dev}1 as BTRFS..."
    mkfs.btrfs -f -L "$label" "${dev}1"

    log_info "  $dev partitioned successfully"
}

run_partitioning() {
    log_header "Executing Partitioning"

    for serial in "${!DISCOVERED_DEVICES[@]}"; do
        local dev="${DISCOVERED_DEVICES[$serial]}"
        local label="${TARGET_LABELS[$serial]}"
        local role="${TARGET_ROLES[$serial]}"

        if [[ "$role" == "primary" ]]; then
            partition_data_drive "$dev" "${BTRFS_LABEL_PREFIX}-${label}"
        elif [[ "$role" == *"esp"* || "$role" == *"boot"* || "$role" == *"system"* || "$role" == *"mirror"* ]]; then
            partition_bootable_drive "$dev" "${BTRFS_LABEL_PREFIX}-${label}"
        else
            partition_data_drive "$dev" "${BTRFS_LABEL_PREFIX}-${label}"
        fi
    done

    log_header "Partitioning Complete"

    echo ""
    echo "Final layout:"
    for serial in "${!DISCOVERED_DEVICES[@]}"; do
        lsblk -o NAME,SIZE,TYPE,FSTYPE,LABEL "${DISCOVERED_DEVICES[$serial]}"
    done
}

# ============================================================================
# MAIN
# ============================================================================

main() {
    local mode="${1:---check}"

    echo "========================================"
    echo "  DAS Drive Partitioning"
    echo "  Date: $(date '+%Y-%m-%d %H:%M:%S')"
    echo "========================================"

    check_root
    discover_devices
    verify_serials

    case "$mode" in
        --check|-c)
            check_smart_tests || true
            show_plan
            echo ""
            log_info "Run with --run to execute partitioning"
            ;;
        --run|-r)
            if ! check_smart_tests; then
                log_error "SMART tests incomplete. Wait or use --force to override."
                exit 1
            fi
            show_plan
            confirm_destruction
            run_partitioning
            ;;
        --force)
            log_warn "Forcing partitioning (SMART tests may be incomplete)"
            show_plan
            confirm_destruction
            run_partitioning
            ;;
        *)
            echo "Usage: $0 [--check|--run|--force]"
            echo "  --check  Verify drives and show plan (default)"
            echo "  --run    Execute partitioning (requires SMART tests complete)"
            echo "  --force  Execute partitioning (skip SMART check)"
            exit 1
            ;;
    esac
}

main "$@"
