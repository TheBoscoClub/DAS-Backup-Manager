#!/usr/bin/env zsh
# das-partition-drives.sh - Partition and format DAS backup drives
# Version: 1.0.0
# Date: 2026-02-03
#
# ⚠️  WARNING: This script DESTROYS ALL DATA on the target drives!
#     Run ONLY after verifying SMART tests passed.
#
# Drive Layout:
#   Drive 1 (sde): Bootable system backup
#     - Partition 1: 1.5G ESP (FAT32) - clone of /boot
#     - Partition 2: ~1998G BTRFS - system subvolumes
#
#   Drive 2 (sdf): HDD critical data backup
#     - Whole disk BTRFS
#
#   Drive 3 (sdh): Mirror of Drive 1 (formatted same as Drive 1)
#   Drive 4 (sdi): Mirror of Drive 2 (formatted same as Drive 2)
#   Drive 5 (sdj): Cold spare (leave unformatted)
#   Drive 6 (sdk): Cold spare / offsite (leave unformatted)
#
# Usage:
#   sudo ./das-partition-drives.sh --check   # Verify drives, show plan
#   sudo ./das-partition-drives.sh --run     # Execute partitioning

set -euo pipefail

# ============================================================================
# CONFIGURATION
# ============================================================================

# Drive assignments (verified by serial number)
DRIVE1="/dev/sde"  # Serial: ZFL41DNY - System backup
DRIVE2="/dev/sdf"  # Serial: ZK208Q7J - Data backup
DRIVE3="/dev/sdh"  # Serial: ZK208Q77 - Mirror of Drive 1
DRIVE4="/dev/sdi"  # Serial: ZFL41DV0 - Mirror of Drive 2

# Expected serials for verification
declare -A EXPECTED_SERIALS=(
    ["/dev/sde"]="ZFL41DNY"
    ["/dev/sdf"]="ZK208Q7J"
    ["/dev/sdh"]="ZK208Q77"
    ["/dev/sdi"]="ZFL41DV0"
)

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

verify_serials() {
    log_header "Verifying Drive Serial Numbers"

    local all_match=true

    for dev in "${!EXPECTED_SERIALS[@]}"; do
        local expected="${EXPECTED_SERIALS[$dev]}"
        local actual

        if [[ ! -b "$dev" ]]; then
            log_error "$dev not found!"
            all_match=false
            continue
        fi

        actual=$(smartctl -i "$dev" 2>/dev/null | awk '/Serial Number:/{print $3; exit}' || echo "UNKNOWN")

        if [[ "$actual" == "$expected" ]]; then
            echo -e "  $dev: ${GREEN}$actual ✓${NC}"
        else
            echo -e "  $dev: ${RED}$actual (expected $expected) ✗${NC}"
            all_match=false
        fi
    done

    if ! $all_match; then
        log_error "Serial number mismatch! Drives may have been reassigned."
        log_error "Check 'lsblk' and update EXPECTED_SERIALS if needed."
        exit 1
    fi

    log_info "All drive serials verified"
}

check_smart_tests() {
    log_header "Checking SMART Test Status"

    local all_complete=true

    for dev in "$DRIVE1" "$DRIVE2" "$DRIVE3" "$DRIVE4"; do
        local status
        status=$(smartctl -l selftest "$dev" 2>/dev/null | grep -E "# 1" | head -1 || echo "No tests")

        if echo "$status" | grep -qE "in progress|Self-test routine in progress"; then
            echo -e "  $dev: ${YELLOW}Test still running${NC}"
            all_complete=false
        elif echo "$status" | grep -q "Completed without error"; then
            echo -e "  $dev: ${GREEN}Test completed - PASSED${NC}"
        else
            echo -e "  $dev: ${YELLOW}$status${NC}"
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
    echo "Drive 1 ($DRIVE1) - Bootable System Backup:"
    echo "  Partition 1: 1.5G ESP (FAT32) - EFI System Partition"
    echo "  Partition 2: remainder BTRFS - label: ${BTRFS_LABEL_PREFIX}-system"
    echo ""
    echo "Drive 2 ($DRIVE2) - Data Backup:"
    echo "  Whole disk BTRFS - label: ${BTRFS_LABEL_PREFIX}-data"
    echo ""
    echo "Drive 3 ($DRIVE3) - Mirror of Drive 1:"
    echo "  Partition 1: 1.5G ESP (FAT32)"
    echo "  Partition 2: remainder BTRFS - label: ${BTRFS_LABEL_PREFIX}-system-mirror"
    echo ""
    echo "Drive 4 ($DRIVE4) - Mirror of Drive 2:"
    echo "  Whole disk BTRFS - label: ${BTRFS_LABEL_PREFIX}-data-mirror"
    echo ""
    echo "Drive 5 & 6: Left unformatted (cold spares)"
}

confirm_destruction() {
    echo ""
    echo -e "${RED}╔══════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${RED}║  ⚠️  WARNING: ALL DATA ON DRIVES 1-4 WILL BE DESTROYED!  ⚠️   ║${NC}"
    echo -e "${RED}╚══════════════════════════════════════════════════════════════╝${NC}"
    echo ""
    echo "Drives to be wiped:"
    echo "  $DRIVE1 ($(lsblk -dn -o SIZE "$DRIVE1"))"
    echo "  $DRIVE2 ($(lsblk -dn -o SIZE "$DRIVE2"))"
    echo "  $DRIVE3 ($(lsblk -dn -o SIZE "$DRIVE3"))"
    echo "  $DRIVE4 ($(lsblk -dn -o SIZE "$DRIVE4"))"
    echo ""
    read -p "Type 'YES-DESTROY' to proceed: " confirm

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

    # Drive 1: Bootable system backup
    partition_bootable_drive "$DRIVE1" "${BTRFS_LABEL_PREFIX}-system"

    # Drive 2: Data backup
    partition_data_drive "$DRIVE2" "${BTRFS_LABEL_PREFIX}-data"

    # Drive 3: Mirror of Drive 1
    partition_bootable_drive "$DRIVE3" "${BTRFS_LABEL_PREFIX}-system-mirror"

    # Drive 4: Mirror of Drive 2
    partition_data_drive "$DRIVE4" "${BTRFS_LABEL_PREFIX}-data-mirror"

    log_header "Partitioning Complete"

    echo ""
    echo "Final layout:"
    lsblk -o NAME,SIZE,TYPE,FSTYPE,LABEL "$DRIVE1" "$DRIVE2" "$DRIVE3" "$DRIVE4"
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
