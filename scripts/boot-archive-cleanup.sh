#!/usr/bin/env zsh
# boot-archive-cleanup.sh - Prune old boot subvolume archives from backup targets
# Version: 1.0.0
# Date: 2026-02-21
#
# When backup-run.sh --full recreates @ and @home, it snapshots the old ones
# as @.archive.YYYYMMDDTHHMMSS before deletion. This script prunes archives
# older than the retention period (default: 365 days).
#
# Usage:
#   sudo ./boot-archive-cleanup.sh              # Prune archives > 365 days old
#   sudo ./boot-archive-cleanup.sh --dryrun     # Preview only
#   sudo ./boot-archive-cleanup.sh --days 180   # Custom retention (180 days)

set -euo pipefail

# ============================================================================
# CONFIGURATION
# ============================================================================

RETENTION_DAYS=365
DRYRUN=false
ARCHIVE_PATTERN="@*.archive.*"

# Mount points for backup targets (same as backup-run.sh)
MOUNT_BACKUP="/mnt/backup-22tb"
MOUNT_BACKUP_SYSTEM="/mnt/backup-system"
MOUNT_BACKUP_SYSTEM_MIRROR="/mnt/backup-system-mirror"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# ============================================================================
# FUNCTIONS
# ============================================================================

log_info()  { echo -e "${GREEN}[INFO]${NC} $1"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
log_error() { echo -e "${RED}[ERROR]${NC} $1"; }

check_root() {
    if [[ $EUID -ne 0 ]]; then
        log_error "This script must be run as root"
        exit 1
    fi
}

# Parse ISO-like timestamp from archive name: @.archive.YYYYMMDDTHHMMSS
parse_archive_timestamp() {
    local name="$1"
    # Extract the timestamp portion after ".archive."
    local ts="${name##*.archive.}"
    if [[ -z "$ts" ]]; then
        echo "0"
        return
    fi
    # Convert YYYYMMDDTHHMMSS to epoch
    local formatted="${ts:0:4}-${ts:4:2}-${ts:6:2}T${ts:9:2}:${ts:11:2}:${ts:13:2}"
    date -d "$formatted" '+%s' 2>/dev/null || echo "0"
}

cleanup_target() {
    local mnt="$1"
    local deleted=0 kept=0 errors=0

    if ! mountpoint -q "$mnt" 2>/dev/null; then
        return
    fi

    local label=$(btrfs filesystem label "$mnt" 2>/dev/null || echo "$mnt")
    log_info "Scanning [$label] for boot archives..."

    local cutoff_epoch=$(( $(date '+%s') - (RETENTION_DAYS * 86400) ))

    # List subvolumes matching archive pattern
    while IFS= read -r line; do
        local subvol_path="${line##* }"  # last field is the path
        local subvol_name="${subvol_path##*/}"

        # Only process archive subvolumes
        [[ "$subvol_name" != *.archive.* ]] && continue

        local archive_epoch=$(parse_archive_timestamp "$subvol_name")
        if (( archive_epoch == 0 )); then
            log_warn "  Could not parse timestamp from: $subvol_name"
            continue
        fi

        if (( archive_epoch < cutoff_epoch )); then
            local age_days=$(( ($(date '+%s') - archive_epoch) / 86400 ))
            if $DRYRUN; then
                log_warn "  [DRYRUN] Would delete: $subvol_path ($age_days days old)"
            else
                if btrfs subvolume delete "$mnt/$subvol_path" 2>/dev/null; then
                    log_info "  Deleted: $subvol_path ($age_days days old)"
                    (( deleted += 1 ))
                else
                    log_error "  Failed to delete: $subvol_path"
                    (( errors += 1 ))
                fi
            fi
        else
            (( kept += 1 ))
        fi
    done < <(btrfs subvolume list "$mnt" 2>/dev/null)

    if $DRYRUN; then
        log_info "  [$label] Would keep $kept, found expired archives above"
    else
        log_info "  [$label] Deleted $deleted, kept $kept, errors $errors"
    fi
}

# ============================================================================
# MAIN
# ============================================================================

main() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --dryrun|-n)
                DRYRUN=true
                ;;
            --days|-d)
                shift
                RETENTION_DAYS="$1"
                ;;
            *)
                echo "Usage: $0 [--dryrun|-n] [--days|-d DAYS]"
                echo "  --dryrun  Preview deletions without acting"
                echo "  --days N  Retention period in days (default: 365)"
                exit 1
                ;;
        esac
        shift
    done

    echo "========================================"
    echo "  Boot Archive Cleanup"
    echo "  Retention: $RETENTION_DAYS days"
    echo "  Mode: $(if $DRYRUN; then echo 'DRYRUN'; else echo 'LIVE'; fi)"
    echo "  Date: $(date '+%Y-%m-%d %H:%M:%S')"
    echo "========================================"
    echo ""

    check_root

    for mnt in "$MOUNT_BACKUP" "$MOUNT_BACKUP_SYSTEM" "$MOUNT_BACKUP_SYSTEM_MIRROR"; do
        cleanup_target "$mnt"
    done

    echo ""
    log_info "Boot archive cleanup complete."
}

main "$@"
