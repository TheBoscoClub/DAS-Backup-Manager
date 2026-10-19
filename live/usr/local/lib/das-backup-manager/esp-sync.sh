#!/bin/bash
# ESP Sync — Structure-aware sync between primary and backup NVMe ESPs
#
# Syncs the systemd-boot ESP from LABEL=EFI to LABEL=EFI-BACKUP using
# file-level comparison. NOT a blind rsync — validates devices via label
# resolution, compares checksums, preserves per-ESP unique files
# (random-seed), and logs every action.
#
# IDENTIFICATION (label-based — survives NVMe renumbering):
#   Primary:  LABEL=EFI        -> /boot
#   Backup:   LABEL=EFI-BACKUP -> /mnt/esp-backup
#   Both must resolve to /dev/nvme* partitions.
#
# FORBIDDEN: DAS drives (LABEL=RECOV-ESP-1 / RECOV-ESP-4, serials SERIAL04 / SERIAL02, /dev/sd*) have independent OS
# installations. NEVER sync host ESP content to DAS drives.
# 2026-03-05 INCIDENT: blind rsync destroyed both DAS emergency boot/recovery ESPs.
# See ~/.claude/rules/esp-ownership.md and DAS-Backup-Manager .claude/rules/esp-safety.md

set -euo pipefail

ESP_PRIMARY="/boot"
ESP_BACKUP="/mnt/esp-backup"
PRIMARY_LABEL="EFI"
BACKUP_LABEL="EFI-BACKUP"
LOG_TAG="esp-sync"
DRY_RUN=false
VERBOSE=false

[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=true && shift
[[ "${1:-}" == "--verbose" || "${1:-}" == "-v" ]] && VERBOSE=true

log()  { logger -t "$LOG_TAG" "$1"; $VERBOSE && echo "$1" || true; }
err()  { logger -t "$LOG_TAG" "ERROR: $1"; echo "ERROR: $1" >&2; }
die()  { err "$1"; exit 1; }

# ── Device validation (CRITICAL SAFETY) ─────────────────────────────
# Resolves expected device from label, then validates the mountpoint
# matches. This approach survives NVMe device renumbering.
validate_device() {
    local mountpoint="$1" expected_label="$2" role="$3"

    # Resolve label to device
    local label_dev
    label_dev=$(blkid -t "LABEL=$expected_label" -o device 2>/dev/null) || true
    if [[ -z "$label_dev" ]]; then
        die "$role: no device found with LABEL=$expected_label"
    fi

    # Get actual mounted device
    local actual_dev
    actual_dev=$(findmnt -n -o SOURCE "$mountpoint" 2>/dev/null) || true
    if [[ -z "$actual_dev" ]]; then
        die "$role: $mountpoint is not mounted"
    fi

    # Resolve symlinks for reliable comparison
    local resolved_actual resolved_label
    resolved_actual=$(readlink -f "$actual_dev")
    resolved_label=$(readlink -f "$label_dev")

    # Cross-check: mounted device must match label resolution
    if [[ "$resolved_actual" != "$resolved_label" ]]; then
        die "$role: $mountpoint is on $actual_dev (resolved: $resolved_actual), but LABEL=$expected_label resolves to $label_dev (resolved: $resolved_label). Mount/label mismatch — REFUSING to sync."
    fi

    # BLOCK: must be an NVMe partition
    if [[ "$resolved_actual" != /dev/nvme* ]]; then
        die "BLOCKED: $resolved_actual is not an NVMe device. ESP sync is NVMe-only. (DAS/USB/SATA devices are forbidden — see 2026-03-05 incident)"
    fi

    # Verify it's vfat
    local fstype
    fstype=$(findmnt -n -o FSTYPE "$mountpoint")
    [[ "$fstype" == "vfat" ]] || die "$role: filesystem is $fstype, expected vfat"

    log "$role: validated $actual_dev (LABEL=$expected_label, $fstype) at $mountpoint"
}

# ── Per-ESP unique files (never sync these) ──────────────────────────
is_unique_file() {
    local relpath="$1"
    case "$relpath" in
        loader/random-seed)     return 0 ;;  # unique per-ESP entropy
        loader/.#bootctl*)      return 0 ;;  # systemd-boot temp files
        test-sync-trigger)      return 0 ;;  # test artifact
    esac
    return 1
}

# ── Ensure both ESPs are mounted ──────────────────────────────────────
if ! mountpoint -q "$ESP_PRIMARY" 2>/dev/null; then
    log "Primary ESP not mounted, attempting mount..."
    mount "$ESP_PRIMARY" 2>/dev/null || die "Failed to mount $ESP_PRIMARY"
fi

if ! mountpoint -q "$ESP_BACKUP" 2>/dev/null; then
    log "Backup ESP not mounted, attempting mount..."
    mount "$ESP_BACKUP" 2>/dev/null || die "Failed to mount $ESP_BACKUP"
fi

# ── Validate both devices ────────────────────────────────────────────
validate_device "$ESP_PRIMARY" "$PRIMARY_LABEL" "Primary"
validate_device "$ESP_BACKUP" "$BACKUP_LABEL" "Backup"

# Verify primary is mounted read-write (we only read from it, but this
# confirms it's a real ESP mount, not rootfs mounted over /boot)
primary_opts=$(findmnt -n -o OPTIONS "$ESP_PRIMARY")
if [[ "$primary_opts" == ro,* || "$primary_opts" == *",ro,"* || "$primary_opts" == *",ro" ]]; then
    die "Primary ESP is mounted read-only — cannot verify integrity"
fi

# ── Sync logic: file-by-file with checksum comparison ────────────────
synced=0
skipped=0
errors=0
removed=0

sync_file() {
    local relpath="$1"
    local src="$ESP_PRIMARY/$relpath"
    local dst="$ESP_BACKUP/$relpath"

    if is_unique_file "$relpath"; then
        log "SKIP (per-ESP unique): $relpath"
        skipped=$((skipped+1))
        return
    fi

    if [[ ! -f "$src" ]]; then
        log "SKIP (source missing): $relpath"
        skipped=$((skipped+1))
        return
    fi

    local needs_copy=false
    if [[ ! -f "$dst" ]]; then
        needs_copy=true
        log "NEW: $relpath"
    else
        local src_md5 dst_md5
        src_md5=$(md5sum "$src" | cut -d' ' -f1)
        dst_md5=$(md5sum "$dst" | cut -d' ' -f1)
        if [[ "$src_md5" != "$dst_md5" ]]; then
            needs_copy=true
            log "CHANGED: $relpath (checksum mismatch)"
        else
            $VERBOSE && log "OK: $relpath (checksums match)"
            skipped=$((skipped+1))
            return
        fi
    fi

    if $needs_copy; then
        if $DRY_RUN; then
            echo "DRY-RUN: would copy $relpath"
        else
            local dstdir
            dstdir=$(dirname "$dst")
            [[ -d "$dstdir" ]] || mkdir -p "$dstdir"
            if cp -a "$src" "$dst"; then
                synced=$((synced+1))
            else
                err "Failed to copy $relpath"
                errors=$((errors+1))
            fi
        fi
    fi
}

# Enumerate all files on primary ESP
log "Scanning primary ESP..."
while IFS= read -r -d '' filepath; do
    relpath="${filepath#"$ESP_PRIMARY"/}"
    sync_file "$relpath"
done < <(find "$ESP_PRIMARY" -type f -print0 | sort -z)

# Check for files on backup that don't exist on primary (stale files)
log "Checking for stale files on backup ESP..."
while IFS= read -r -d '' filepath; do
    relpath="${filepath#"$ESP_BACKUP"/}"
    if is_unique_file "$relpath"; then
        continue
    fi
    if [[ ! -f "$ESP_PRIMARY/$relpath" ]]; then
        if $DRY_RUN; then
            echo "DRY-RUN: would remove stale $relpath"
        else
            log "REMOVE (stale): $relpath"
            rm -f -- "${filepath:?}"
            removed=$((removed+1))
        fi
    fi
done < <(find "$ESP_BACKUP" -type f -print0 | sort -z)

# Clean empty directories on backup
if ! $DRY_RUN; then
    find "${ESP_BACKUP:?}" -type d -empty -delete 2>/dev/null || true
fi

# ── Summary ──────────────────────────────────────────────────────────
summary="ESP sync complete: $synced copied, $skipped unchanged, $removed removed, $errors errors"
logger -t "$LOG_TAG" "$summary"
echo "$summary"

[[ $errors -gt 0 ]] && exit 1
exit 0
