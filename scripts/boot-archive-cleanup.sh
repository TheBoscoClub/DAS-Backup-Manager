#!/bin/bash
# boot-archive-cleanup.sh - Prune old boot subvolume archives from backup targets (config-driven)
# Version: 2.1.0
# Date: 2026-08-02
#
# When backup-run.sh --full (or the Rust btrdasd manual path) recreates @ and
# @home, it snapshots the old ones as @.archive.YYYYMMDDTHHMMSS before
# deletion. This script prunes archives older than the retention period (from
# config.toml, default: 60 days). All configuration loaded from config.toml
# via btrdasd. As of v4.2.4, backup-run.sh invokes this script automatically
# at the end of every run (daily and full) while targets are still mounted —
# it was previously installed but never called by anything (DAS-Backup-Manager-64h).
#
# v2.1.0: role=mirror targets (e.g. the recovery-A/B 2TB drives, which carry a
# genuinely independent OS install in their own @/@home) are skipped entirely.
# On a mirror, a @.archive.* snapshot may be the LAST surviving copy of that
# independent install if the Rust archive_boot() path ever clobbered live @
# before it learned to skip mirrors (bd DAS-Backup-Manager-am1) — pruning it
# after 60 days would make that loss unrecoverable. Mirror-role skip logic and
# wording mirror update_boot_subvolumes() further down in the sibling
# backup-run.sh, both driven by the same `btrdasd config dump-env` ROLE field.
#
# Usage:
#   sudo ./boot-archive-cleanup.sh              # Prune archives past retention
#   sudo ./boot-archive-cleanup.sh --dryrun     # Preview only
#   sudo ./boot-archive-cleanup.sh --days 180   # Override retention (180 days)

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

RETENTION_DAYS="$DAS_BOOT_ARCHIVE_RETENTION_DAYS"
DRYRUN=false
# All target mount points from config
IFS=' ' read -ra ALL_TARGET_MOUNTS <<< "$DAS_ALL_TARGET_MOUNTS"
# Fail closed on an empty target list. A zero-element array makes the main loop
# a no-op and the run then reports "cleanup complete" having examined nothing —
# indistinguishable from a genuine clean prune.
if [[ ${#ALL_TARGET_MOUNTS[@]} -eq 0 ]]; then
    echo "ERROR: DAS_ALL_TARGET_MOUNTS is empty - no targets to examine (check $DAS_CONFIG)" >&2
    exit 1
fi

# Failure accounting - the process exit status must reflect these (see main()).
DELETE_ERRORS=0     # subvolume deletions that failed
TARGET_FAILURES=0   # targets whose subvolume listing could not be obtained

# Mount -> role map, built the same way backup-run.sh builds MOUNT_ROLES —
# reused here so the pruner and the archiver agree on which targets are
# mirrors without a second source of truth (bd DAS-Backup-Manager-am1).
declare -A MOUNT_ROLES=()
for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
    mount_var="DAS_TARGET_${i}_MOUNT"
    role_var="DAS_TARGET_${i}_ROLE"
    MOUNT_ROLES[${!mount_var}]="${!role_var}"
done

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
        log_info "  Skipping $mnt (not mounted - nothing examined)"
        return 0
    fi

    local label
    label=$(btrfs filesystem label "$mnt" 2>/dev/null || echo "$mnt")

    # Never prune archives on mirror targets — their @.archive.* snapshots may
    # be the only surviving copy of that target's independent OS install.
    # Same skip condition and wording as update_boot_subvolumes() in
    # backup-run.sh (bd DAS-Backup-Manager-am1).
    local mount_role="${MOUNT_ROLES[$mnt]:-}"
    if [[ "$mount_role" == "mirror" ]]; then
        log_info "  [$label] Skipping mirror target (independent OS)"
        return
    fi

    log_info "Scanning [$label] for boot archives..."

    local cutoff_epoch=$(( $(date '+%s') - (RETENTION_DAYS * 86400) ))

    # Obtain the subvolume listing FIRST and check that it succeeded. Feeding the
    # loop directly from a process substitution hid listing failures completely:
    # the loop body simply never ran, the counters stayed at zero, and the target
    # reported "Deleted 0, kept 0, errors 0" — a listing failure is not a clean
    # prune and must never be reportable as one.
    local listing listing_stderr listing_err
    listing_stderr=$(mktemp)
    if ! listing=$(btrfs subvolume list "$mnt" 2>"$listing_stderr"); then
        listing_err=$(tr '\n' ' ' < "$listing_stderr")
        rm -f "$listing_stderr"
        log_error "  [$label] Failed to list subvolumes: ${listing_err:-(no error output)}"
        log_error "  [$label] Target NOT pruned - state unknown"
        TARGET_FAILURES=$(( TARGET_FAILURES + 1 ))
        return 0
    fi
    rm -f "$listing_stderr"

    # Process archive subvolumes from the (successfully obtained) listing
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue

        local subvol_path="${line##* }"  # last field is the path
        local subvol_name="${subvol_path##*/}"

        # Only process archive subvolumes
        [[ "$subvol_name" != *.archive.* ]] && continue

        # This string is about to be handed to `btrfs subvolume delete`, so it
        # must be validated first. "${line##* }" is a last-space-separated-field
        # parse of human-readable output; anything that does not come back as a
        # clean, relative, exactly-shaped archive path is skipped, never deleted.
        # The first test cross-checks the last-field parse against the field
        # `btrfs subvolume list` actually labels "path" — they differ when the
        # line is malformed or the path itself contains a space, in which case
        # the last-field parse names a DIFFERENT subvolume than the listing did.
        if [[ "${line#*" path "}" != "$subvol_path" ]] \
           || [[ "$subvol_path" == "$line" ]] \
           || [[ "$subvol_path" == /* ]] \
           || [[ "$subvol_path" == *".."* ]] \
           || [[ ! "$subvol_path" =~ ^[A-Za-z0-9_@.+/-]+$ ]] \
           || [[ ! "$subvol_name" =~ ^@[A-Za-z0-9_-]*\.archive\.[0-9]{8}T[0-9]{6}$ ]]; then
            log_warn "  Unrecognized archive path - NOT deleted: $subvol_path"
            continue
        fi

        local archive_epoch
        archive_epoch=$(parse_archive_timestamp "$subvol_name")
        if (( archive_epoch == 0 )); then
            log_warn "  Could not parse timestamp from: $subvol_name"
            continue
        fi

        if (( archive_epoch < cutoff_epoch )); then
            local age_days=$(( ($(date '+%s') - archive_epoch) / 86400 ))
            if $DRYRUN; then
                log_warn "  [DRYRUN] Would delete: $subvol_path ($age_days days old)"
            else
                local delete_err
                # 2>&1 >/dev/null keeps stderr only: the errno text is the whole
                # point of the failure line and used to be discarded.
                if delete_err=$(btrfs subvolume delete "$mnt/$subvol_path" 2>&1 >/dev/null); then
                    log_info "  Deleted: $subvol_path ($age_days days old)"
                    (( deleted += 1 ))
                else
                    log_error "  Failed to delete: $subvol_path: ${delete_err//$'\n'/ }"
                    (( errors += 1 ))
                fi
            fi
        else
            (( kept += 1 ))
        fi
    done <<< "$listing"

    DELETE_ERRORS=$(( DELETE_ERRORS + errors ))

    if $DRYRUN; then
        log_info "  [$label] Would keep $kept, found expired archives above"
    else
        log_info "  [$label] Deleted $deleted, kept $kept, errors $errors"
    fi
    return 0
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
                echo "  --days N  Override retention period (default from config: $DAS_BOOT_ARCHIVE_RETENTION_DAYS days)"
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

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        cleanup_target "$mnt"
    done

    echo ""
    # The exit status must reflect what actually happened. Ending on a log_info
    # made every run exit 0, including runs where every target's listing failed
    # or every deletion failed. Exit 0 still means "ran fine, nothing wrong" —
    # including "ran fine, nothing to delete".
    if (( TARGET_FAILURES > 0 || DELETE_ERRORS > 0 )); then
        log_error "Boot archive cleanup FAILED: $DELETE_ERRORS deletion error(s), $TARGET_FAILURES target(s) not examined."
        return 1
    fi

    log_info "Boot archive cleanup complete."
    return 0
}

main "$@"
