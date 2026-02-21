#!/usr/bin/env zsh
# backup-run.sh - Run btrbk backup to DAS 22TB drive
# Version: 3.1.0
# Date: 2026-02-19
#
# Features:
#   - Incremental BTRFS backups via btrbk to 22TB Exos primary drive
#   - Maintains stable boot subvolumes (@ and @home) for disaster recovery
#   - Syncs ESP to 2TB bootable recovery drives (Bay 1 + Bay 6)
#   - Detects DAS drives by serial number (stable across reboots)
#   - Logs per-target throughput (data written + MB/s rate)
#   - Designed for unattended nightly execution
#
# Prerequisites:
#   - DAS connected and powered on
#   - btrbk.conf installed at /etc/btrbk/btrbk.conf
#   - 22TB drive partitioned (p1: whole-disk BTRFS, no ESP)
#
# Usage:
#   sudo ./backup-run.sh              # Incremental backup
#   sudo ./backup-run.sh --dryrun     # Preview only
#   sudo ./backup-run.sh --full       # Force full backup (recreate boot subvols)

set -euo pipefail
zmodload zsh/datetime  # provides $EPOCHSECONDS for throughput timing

# ============================================================================
# CONFIGURATION
# ============================================================================

# DAS drive identification by serial number (stable across reboots)
declare -A DAS_SERIALS=(
    ["primary"]="ZXA0LMAE"        # 22TB Exos - primary backup target (Bay 2)
    ["system_2tb"]="ZFL41DNY"     # 2TB old system backup (Bay 6) - ESP sync only
    ["system_mirror"]="ZK208Q77"  # 2TB system mirror (Bay 1) - ESP sync only
)

# Mount points for source top-level volumes
MOUNT_NVME="/.btrfs-nvme"
MOUNT_SSD="/.btrfs-ssd"
MOUNT_HDD="/.btrfs-hdd"

# Mount point for 22TB backup target (unified — all btrbk targets live here)
MOUNT_BACKUP="/mnt/backup-22tb"

# Mount points for 2TB bootable recovery drives (secondary btrbk targets for NVMe+SSD)
MOUNT_BACKUP_SYSTEM="/mnt/backup-system"
MOUNT_BACKUP_SYSTEM_MIRROR="/mnt/backup-system-mirror"

# DAS ESP mount points (up to 3 ESPs to sync)
MOUNT_DAS_ESP=("/mnt/das-esp-1" "/mnt/das-esp-2" "/mnt/das-esp-3")

# Source devices
DEV_NVME="/dev/nvme0n1p2"
DEV_SSD="/dev/sdb"
DEV_HDD="/dev/sda"

# Logging
LOG_FILE="/var/log/das-backup.log"

# Throughput tracking (populated at runtime)
declare -A USAGE_BEFORE=()
declare -A USAGE_AFTER=()
BTRBK_START_TIME=0
BTRBK_END_TIME=0

# Operation status tracking (for email report)
declare -A OP_STATUS=()

# Email and growth tracking
EMAIL_CONF="/etc/das-backup-email.conf"
GROWTH_LOG="/var/lib/das-backup/growth.log"
LAST_REPORT="/var/lib/das-backup/last-report.txt"

# Target display names (populated after mount_targets)
declare -A TARGET_NAMES=()

# Colors for interactive output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# ============================================================================
# FUNCTIONS
# ============================================================================

log() {
    local level="$1"
    local msg="$2"
    local timestamp=$(date '+%Y-%m-%d %H:%M:%S')
    echo "[$timestamp] [$level] $msg" >> "$LOG_FILE"

    case "$level" in
        INFO)  echo -e "${GREEN}[INFO]${NC} $msg" ;;
        WARN)  echo -e "${YELLOW}[WARN]${NC} $msg" ;;
        ERROR) echo -e "${RED}[ERROR]${NC} $msg" ;;
    esac
}

log_info()  { log "INFO" "$1"; }
log_warn()  { log "WARN" "$1"; }
log_error() { log "ERROR" "$1"; }

# Track operation status for the email report
record_op() {
    local op="$1" result="$2" detail="${3:-}"
    OP_STATUS[$op]="$result"
    if [[ -n "$detail" ]]; then
        OP_STATUS["${op}_detail"]="$detail"
    fi
}

check_root() {
    if [[ $EUID -ne 0 ]]; then
        log_error "This script must be run as root"
        exit 1
    fi
}

# Find device by serial number
find_device_by_serial() {
    local serial="$1"
    for dev in /dev/sd[a-z](N) /dev/sd[a-z][a-z](N); do
        if [[ -b "$dev" ]]; then
            local dev_serial=$(smartctl -i "$dev" 2>/dev/null | awk '/Serial Number:/{print $3}')
            if [[ "$dev_serial" == "$serial" ]]; then
                echo "$dev"
                return 0
            fi
        fi
    done
    return 1
}

check_das_connected() {
    log_info "Detecting DAS drives by serial number..."

    DRIVE_PRIMARY=$(find_device_by_serial "${DAS_SERIALS[primary]}") || {
        log_error "22TB primary backup drive (${DAS_SERIALS[primary]}) not found"
        log_error "Is the DAS connected and powered on?"
        exit 1
    }
    log_info "  Primary (22TB): $DRIVE_PRIMARY (${DAS_SERIALS[primary]})"

    # Optional: detect ESP-only drives for sync
    DRIVE_SYSTEM_2TB=$(find_device_by_serial "${DAS_SERIALS[system_2tb]}") || DRIVE_SYSTEM_2TB=""
    DRIVE_SYSTEM_MIRROR=$(find_device_by_serial "${DAS_SERIALS[system_mirror]}") || DRIVE_SYSTEM_MIRROR=""

    if [[ -n "$DRIVE_SYSTEM_2TB" ]]; then
        log_info "  System 2TB: $DRIVE_SYSTEM_2TB (${DAS_SERIALS[system_2tb]}) — ESP sync"
    fi
    if [[ -n "$DRIVE_SYSTEM_MIRROR" ]]; then
        log_info "  System mirror: $DRIVE_SYSTEM_MIRROR (${DAS_SERIALS[system_mirror]}) — ESP sync"
    fi
}

set_io_scheduler() {
    log_info "Setting I/O scheduler to mq-deadline for DAS drives..."

    for drive in "$DRIVE_PRIMARY" "$DRIVE_SYSTEM_2TB" "$DRIVE_SYSTEM_MIRROR"; do
        if [[ -n "$drive" && -b "$drive" ]]; then
            local dev="${drive#/dev/}"
            if [[ -f "/sys/block/$dev/queue/scheduler" ]]; then
                echo mq-deadline > "/sys/block/$dev/queue/scheduler" 2>/dev/null || true
            fi
        fi
    done
}

create_mount_points() {
    log_info "Creating mount points..."
    mkdir -p "$MOUNT_NVME" "$MOUNT_SSD" "$MOUNT_HDD"
    mkdir -p "$MOUNT_BACKUP"
    for esp in "${MOUNT_DAS_ESP[@]}"; do
        mkdir -p "$esp"
    done
}

mount_sources() {
    log_info "Mounting source top-level volumes..."

    if ! mountpoint -q "$MOUNT_NVME"; then
        mount -o subvolid=5 "$DEV_NVME" "$MOUNT_NVME"
        log_info "  Mounted NVMe at $MOUNT_NVME"
    fi

    if ! mountpoint -q "$MOUNT_SSD"; then
        mount -o subvolid=5 "$DEV_SSD" "$MOUNT_SSD"
        log_info "  Mounted SSD at $MOUNT_SSD"
    fi

    if ! mountpoint -q "$MOUNT_HDD"; then
        mount -o subvolid=5 "$DEV_HDD" "$MOUNT_HDD"
        log_info "  Mounted HDD at $MOUNT_HDD"
    fi
}

mount_targets() {
    log_info "Mounting backup targets..."

    # HDD-optimized options for USB 3.2
    local mount_opts="nossd,noatime,space_cache=v2,commit=120"

    # Primary: 22TB Exos (required)
    if ! mountpoint -q "$MOUNT_BACKUP"; then
        if [[ ! -b "${DRIVE_PRIMARY}1" ]]; then
            log_error "Partition ${DRIVE_PRIMARY}1 not found — is the 22TB drive partitioned?"
            exit 1
        fi
        mount -o "$mount_opts" "${DRIVE_PRIMARY}1" "$MOUNT_BACKUP"
        log_info "  Mounted 22TB backup at $MOUNT_BACKUP"
    fi

    # Secondary: 2TB bootable recovery drives (optional — keeps recovery OS current)
    if [[ -n "$DRIVE_SYSTEM_2TB" ]]; then
        mkdir -p "$MOUNT_BACKUP_SYSTEM"
        if [[ -b "${DRIVE_SYSTEM_2TB}2" ]] && ! mountpoint -q "$MOUNT_BACKUP_SYSTEM"; then
            mount -o "$mount_opts" "${DRIVE_SYSTEM_2TB}2" "$MOUNT_BACKUP_SYSTEM" && \
                log_info "  Mounted 2TB system backup at $MOUNT_BACKUP_SYSTEM" || \
                log_warn "  Could not mount 2TB system backup — btrbk will skip it"
        fi
    fi

    if [[ -n "$DRIVE_SYSTEM_MIRROR" ]]; then
        mkdir -p "$MOUNT_BACKUP_SYSTEM_MIRROR"
        if [[ -b "${DRIVE_SYSTEM_MIRROR}2" ]] && ! mountpoint -q "$MOUNT_BACKUP_SYSTEM_MIRROR"; then
            mount -o "$mount_opts" "${DRIVE_SYSTEM_MIRROR}2" "$MOUNT_BACKUP_SYSTEM_MIRROR" && \
                log_info "  Mounted 2TB system mirror at $MOUNT_BACKUP_SYSTEM_MIRROR" || \
                log_warn "  Could not mount 2TB system mirror — btrbk will skip it"
        fi
    fi
}

create_snapshot_dirs() {
    log_info "Creating btrbk snapshot directories..."
    mkdir -p "$MOUNT_NVME/.btrbk-snapshots"
    mkdir -p "$MOUNT_SSD/.btrbk-snapshots"
    mkdir -p "$MOUNT_HDD/ClaudeCodeProjects/.btrbk-snapshots"
    mkdir -p "$MOUNT_HDD/Audiobooks/.btrbk-snapshots"
}

create_target_dirs() {
    log_info "Creating target directory structure..."
    mkdir -p "$MOUNT_BACKUP/nvme"
    mkdir -p "$MOUNT_BACKUP/ssd"
    mkdir -p "$MOUNT_BACKUP/projects"
    mkdir -p "$MOUNT_BACKUP/audiobooks"
    mkdir -p "$MOUNT_BACKUP/storage"

    # 2TB bootable recovery drives get NVMe + SSD targets only
    for mnt in "$MOUNT_BACKUP_SYSTEM" "$MOUNT_BACKUP_SYSTEM_MIRROR"; do
        if mountpoint -q "$mnt" 2>/dev/null; then
            mkdir -p "$mnt/nvme"
            mkdir -p "$mnt/ssd"
        fi
    done
}

run_btrbk() {
    local mode="${1:-run}"

    log_info "Running btrbk ($mode)..."

    if [[ "$mode" == "dryrun" ]]; then
        btrbk -c /etc/btrbk/btrbk.conf dryrun
        record_op "btrbk" "OK" "dryrun"
    else
        if btrbk -c /etc/btrbk/btrbk.conf run; then
            record_op "btrbk" "OK"
            log_info "btrbk completed"
        else
            record_op "btrbk" "FAIL" "exit code $?"
            log_error "btrbk failed"
        fi
    fi
}

update_boot_subvolumes() {
    local force="${1:-false}"
    local updated=0 skipped=0 failed=0

    log_info "Updating stable boot subvolumes..."

    # Update boot subvolumes on each mounted backup target that has NVMe snapshots
    for mnt in "$MOUNT_BACKUP" "$MOUNT_BACKUP_SYSTEM" "$MOUNT_BACKUP_SYSTEM_MIRROR"; do
        if ! mountpoint -q "$mnt" 2>/dev/null; then
            continue
        fi

        local label=$(btrfs filesystem label "$mnt" 2>/dev/null || echo "$mnt")
        local latest_root=$(btrfs subvolume list "$mnt" | grep "nvme/root\." | awk '{print $NF}' | sort | tail -1)
        local latest_home=$(btrfs subvolume list "$mnt" | grep "nvme/home\." | awk '{print $NF}' | sort | tail -1)

        if [[ -z "$latest_root" || -z "$latest_home" ]]; then
            log_warn "  [$label] No btrbk snapshots found, skipping"
            (( skipped += 1 ))
            continue
        fi

        log_info "  [$label] Latest root: $latest_root"
        log_info "  [$label] Latest home: $latest_home"

        # Update @ subvolume (create-then-swap to avoid power-loss window)
        if btrfs subvolume show "$mnt/@" &>/dev/null; then
            if [[ "$force" == "true" ]]; then
                if btrfs subvolume snapshot "$mnt/$latest_root" "$mnt/@.new" && \
                   btrfs subvolume delete "$mnt/@" && \
                   mv "$mnt/@.new" "$mnt/@"; then
                    log_info "  [$label] Recreated @ from $latest_root"
                    (( updated += 1 ))
                else
                    log_error "  [$label] Failed to recreate @"
                    (( failed += 1 ))
                fi
            else
                log_info "  [$label] @ exists, skipping (use --full to recreate)"
                (( skipped += 1 ))
            fi
        else
            if btrfs subvolume snapshot "$mnt/$latest_root" "$mnt/@"; then
                log_info "  [$label] Created @ from $latest_root"
                (( updated += 1 ))
            else
                log_error "  [$label] Failed to create @"
                (( failed += 1 ))
            fi
        fi

        # Update @home subvolume (create-then-swap to avoid power-loss window)
        if btrfs subvolume show "$mnt/@home" &>/dev/null; then
            if [[ "$force" == "true" ]]; then
                if btrfs subvolume snapshot "$mnt/$latest_home" "$mnt/@home.new" && \
                   btrfs subvolume delete "$mnt/@home" && \
                   mv "$mnt/@home.new" "$mnt/@home"; then
                    log_info "  [$label] Recreated @home from $latest_home"
                    (( updated += 1 ))
                else
                    log_error "  [$label] Failed to recreate @home"
                    (( failed += 1 ))
                fi
            else
                log_info "  [$label] @home exists, skipping (use --full to recreate)"
            fi
        else
            if btrfs subvolume snapshot "$mnt/$latest_home" "$mnt/@home"; then
                log_info "  [$label] Created @home from $latest_home"
                (( updated += 1 ))
            else
                log_error "  [$label] Failed to create @home"
                (( failed += 1 ))
            fi
        fi
    done

    if (( failed > 0 )); then
        record_op "boot_subvols" "FAIL" "$updated updated, $failed failed"
    else
        record_op "boot_subvols" "OK" "$updated updated, $skipped skipped"
    fi
}

sync_das_esp() {
    log_info "Syncing ESP to DAS backup drives..."

    local esp_source="/boot"
    local esp_idx=0
    local esp_ok=0 esp_fail=0

    # Build list of ESP partitions to sync (2TB bootable recovery drives only)
    # 22TB has no ESP — it's a pure backup/storage drive
    local esp_parts=()
    [[ -n "$DRIVE_SYSTEM_2TB" && -b "${DRIVE_SYSTEM_2TB}1" ]] && esp_parts+=("${DRIVE_SYSTEM_2TB}1")
    [[ -n "$DRIVE_SYSTEM_MIRROR" && -b "${DRIVE_SYSTEM_MIRROR}1" ]] && esp_parts+=("${DRIVE_SYSTEM_MIRROR}1")

    local esp_total=${#esp_parts[@]}

    for esp_part in "${esp_parts[@]}"; do
        local mount_point="${MOUNT_DAS_ESP[$((esp_idx + 1))]}"
        esp_idx=$((esp_idx + 1))

        if ! mountpoint -q "$mount_point"; then
            mount "$esp_part" "$mount_point" 2>/dev/null || {
                log_warn "Could not mount ESP $esp_part"
                (( esp_fail += 1 ))
                continue
            }
        fi

        if rsync -aHAX --delete \
            --exclude='loader/random-seed' \
            "$esp_source/" "$mount_point/" 2>/dev/null; then
            log_info "  Synced ESP to $esp_part"
            (( esp_ok += 1 ))
        else
            log_warn "  ESP sync to $esp_part failed"
            (( esp_fail += 1 ))
        fi

        umount "$mount_point" 2>/dev/null || true
    done

    if (( esp_fail > 0 )); then
        record_op "esp_sync" "FAIL" "$esp_ok of $esp_total synced, $esp_fail failed"
    else
        record_op "esp_sync" "OK" "$esp_ok of $esp_total ESPs synced"
    fi
}

unmount_all() {
    log_info "Unmounting volumes..."

    for esp in "${MOUNT_DAS_ESP[@]}"; do
        umount "$esp" 2>/dev/null || true
    done
    umount "$MOUNT_BACKUP_SYSTEM_MIRROR" 2>/dev/null || true
    umount "$MOUNT_BACKUP_SYSTEM" 2>/dev/null || true
    umount "$MOUNT_BACKUP" 2>/dev/null || true
    umount "$MOUNT_HDD" 2>/dev/null || true
    umount "$MOUNT_SSD" 2>/dev/null || true
    umount "$MOUNT_NVME" 2>/dev/null || true

    log_info "All volumes unmounted"
}

show_stats() {
    log_info "Backup statistics:"
    btrbk -c /etc/btrbk/btrbk.conf list latest 2>/dev/null || true

    log_info "Target disk usage:"
    df -h "$MOUNT_BACKUP" 2>/dev/null || true
}

# Get used bytes on a mounted filesystem
get_used_bytes() {
    df --output=used -B1 "$1" 2>/dev/null | tail -1 | tr -d ' '
}

# Format byte count to human-readable string
format_bytes() {
    local bytes=$1
    if (( bytes >= 1073741824 )); then
        printf "%.2f GiB" "$(( bytes / 1073741824.0 ))"
    elif (( bytes >= 1048576 )); then
        printf "%.2f MiB" "$(( bytes / 1048576.0 ))"
    elif (( bytes >= 1024 )); then
        printf "%.2f KiB" "$(( bytes / 1024.0 ))"
    else
        printf "%d B" "$bytes"
    fi
}

# Capture disk usage on all mounted backup targets
capture_usage() {
    local phase="$1"  # "before" or "after"

    for mnt in "$MOUNT_BACKUP" "$MOUNT_BACKUP_SYSTEM" "$MOUNT_BACKUP_SYSTEM_MIRROR"; do
        if mountpoint -q "$mnt" 2>/dev/null; then
            local used=$(get_used_bytes "$mnt")
            if [[ "$phase" == "before" ]]; then
                USAGE_BEFORE[$mnt]=$used
            else
                USAGE_AFTER[$mnt]=$used
            fi
        fi
    done

    if [[ "$phase" == "before" ]]; then
        BTRBK_START_TIME=$EPOCHSECONDS
    else
        BTRBK_END_TIME=$EPOCHSECONDS
    fi
}

# Log throughput report for all targets
log_throughput() {
    local elapsed=$(( BTRBK_END_TIME - BTRBK_START_TIME ))
    local elapsed_min=$(( elapsed / 60 ))
    local elapsed_sec=$(( elapsed % 60 ))

    log_info "=== Throughput Report ==="
    log_info "  Elapsed: ${elapsed_min}m ${elapsed_sec}s"

    local total_written=0

    for mnt in "$MOUNT_BACKUP" "$MOUNT_BACKUP_SYSTEM" "$MOUNT_BACKUP_SYSTEM_MIRROR"; do
        local name="${TARGET_NAMES[$mnt]}"
        local before="${USAGE_BEFORE[$mnt]:-}"
        local after="${USAGE_AFTER[$mnt]:-}"

        if [[ -z "$before" || -z "$after" ]]; then
            continue
        fi

        local delta=$(( after - before ))
        total_written=$(( total_written + delta ))

        if (( delta <= 0 )); then
            log_info "  $name: no new data written"
        elif (( elapsed > 0 )); then
            local rate_bytes=$(( delta / elapsed ))
            log_info "  $name: $(format_bytes $delta) @ $(format_bytes $rate_bytes)/s"
        else
            log_info "  $name: $(format_bytes $delta)"
        fi
    done

    if (( total_written > 0 && elapsed > 0 )); then
        local total_rate=$(( total_written / elapsed ))
        log_info "  ─────────────────────────────────────"
        log_info "  Total: $(format_bytes $total_written) @ $(format_bytes $total_rate)/s"
    fi
}

# ============================================================================
# GROWTH TRACKING
# ============================================================================

# Append current usage to the growth log for trend analysis
record_growth() {
    mkdir -p "$(dirname "$GROWTH_LOG")"
    local ts=$(date '+%Y-%m-%dT%H:%M:%S')

    for mnt in "$MOUNT_BACKUP" "$MOUNT_BACKUP_SYSTEM" "$MOUNT_BACKUP_SYSTEM_MIRROR"; do
        if mountpoint -q "$mnt" 2>/dev/null; then
            local used=$(get_used_bytes "$mnt")
            echo "$ts $mnt $used" >> "$GROWTH_LOG"
        fi
    done
}

# Compute average daily growth over a lookback period (returns bytes/day)
compute_growth_stats() {
    local mnt="$1"
    local current_used="$2"
    local lookback_days="$3"

    if [[ ! -f "$GROWTH_LOG" ]]; then
        echo "0"
        return
    fi

    local now=$EPOCHSECONDS
    local cutoff=$(( now - (lookback_days * 86400) ))

    # Find the oldest entry for this mount within the lookback window
    local oldest_epoch=0 oldest_used=0
    while IFS=' ' read -r ts entry_mnt entry_used; do
        [[ "$entry_mnt" != "$mnt" ]] && continue
        # Parse ISO timestamp to epoch
        local entry_epoch=$(date -d "$ts" '+%s' 2>/dev/null || echo 0)
        if (( entry_epoch >= cutoff && oldest_epoch == 0 )); then
            oldest_epoch=$entry_epoch
            oldest_used=$entry_used
            break
        fi
    done < "$GROWTH_LOG"

    if (( oldest_epoch == 0 )); then
        echo "0"
        return
    fi

    local actual_days=$(( (now - oldest_epoch) / 86400 ))
    if (( actual_days <= 0 )); then
        echo "0"
        return
    fi

    local delta=$(( current_used - oldest_used ))
    if (( delta < 0 )); then delta=0; fi

    echo $(( delta / actual_days ))
}

# ============================================================================
# SMART SUMMARY
# ============================================================================

# Get quick SMART data for a drive (returns "health|temp|hours|serial")
get_smart_summary() {
    local dev="$1"
    local health temp hours serial

    health=$(smartctl -H "$dev" 2>/dev/null | grep -o "PASSED\|FAILED" || echo "UNKNOWN")
    temp=$(smartctl -A "$dev" 2>/dev/null | awk '/Temperature_Celsius/{print $10}' || echo "?")
    hours=$(smartctl -A "$dev" 2>/dev/null | awk '/Power_On_Hours/{print $10}' || echo "?")
    serial=$(smartctl -i "$dev" 2>/dev/null | awk '/Serial Number:/{print $3}' || echo "?")

    echo "${health}|${temp}|${hours}|${serial}"
}

# ============================================================================
# EMAIL REPORT
# ============================================================================

generate_report() {
    local hostname=$(hostname)
    local timestamp=$(date '+%Y-%m-%d %H:%M')
    local overall_status="ALL OPERATIONS SUCCESSFUL"

    # Check for any failures
    for op in "${(@k)OP_STATUS}"; do
        if [[ "${OP_STATUS[$op]}" == "FAIL" ]]; then
            overall_status="FAILURES DETECTED"
            break
        fi
    done

    local elapsed=$(( BTRBK_END_TIME - BTRBK_START_TIME ))
    local elapsed_min=$(( elapsed / 60 ))
    local elapsed_sec=$(( elapsed % 60 ))

    # Build the report
    cat <<-REPORT
===============================================================
  DAS Backup Report — $timestamp
  Host: $hostname
  Status: $overall_status
===============================================================

BACKUP OPERATIONS
───────────────────────────────────────────────────────────────
  btrbk send/receive    ${OP_STATUS[btrbk]:-N/A}  (${elapsed_min}m ${elapsed_sec}s)
  Boot subvolumes       ${OP_STATUS[boot_subvols]:-N/A}  (${OP_STATUS[boot_subvols_detail]:-n/a})
  ESP mirror            ${OP_STATUS[esp_sync]:-N/A}  (${OP_STATUS[esp_sync_detail]:-n/a})

THROUGHPUT
───────────────────────────────────────────────────────────────
$(generate_throughput_section)

DISK CAPACITY
───────────────────────────────────────────────────────────────
$(generate_capacity_section)

GROWTH ANALYSIS
───────────────────────────────────────────────────────────────
$(generate_growth_section)

SMART STATUS
───────────────────────────────────────────────────────────────
$(generate_smart_section)

LATEST SNAPSHOTS
───────────────────────────────────────────────────────────────
$(btrbk -c /etc/btrbk/btrbk.conf list latest 2>/dev/null | awk 'NR>1{printf "  %s\n", $0}' || echo "  (none yet)")

===============================================================
  backup-run.sh v3.1.0
  Next scheduled: $(systemctl show das-backup.timer --property=NextElapseUSecRealtime 2>/dev/null | cut -d= -f2 | sed 's/ [A-Z]*$//' || echo "unknown")
===============================================================
REPORT
}

generate_throughput_section() {
    local elapsed=$(( BTRBK_END_TIME - BTRBK_START_TIME ))
    local total_written=0

    for mnt in "$MOUNT_BACKUP" "$MOUNT_BACKUP_SYSTEM" "$MOUNT_BACKUP_SYSTEM_MIRROR"; do
        local name="${TARGET_NAMES[$mnt]}"
        local before="${USAGE_BEFORE[$mnt]:-}"
        local after="${USAGE_AFTER[$mnt]:-}"

        if [[ -z "$before" || -z "$after" ]]; then continue; fi

        local delta=$(( after - before ))
        total_written=$(( total_written + delta ))

        if (( delta <= 0 )); then
            printf "  %-24s no new data\n" "$name"
        elif (( elapsed > 0 )); then
            local rate=$(( delta / elapsed ))
            printf "  %-24s %s @ %s/s\n" "$name" "$(format_bytes $delta)" "$(format_bytes $rate)"
        else
            printf "  %-24s %s\n" "$name" "$(format_bytes $delta)"
        fi
    done

    if (( total_written > 0 && elapsed > 0 )); then
        local total_rate=$(( total_written / elapsed ))
        printf "  ─────────────────────────────────────\n"
        printf "  %-24s %s @ %s/s\n" "Total" "$(format_bytes $total_written)" "$(format_bytes $total_rate)"
    fi
}

generate_capacity_section() {
    printf "  %-24s %-10s %-10s %s\n" "Target" "Used" "Avail" "Use%"

    for mnt in "$MOUNT_BACKUP" "$MOUNT_BACKUP_SYSTEM" "$MOUNT_BACKUP_SYSTEM_MIRROR"; do
        if ! mountpoint -q "$mnt" 2>/dev/null; then continue; fi
        local name="${TARGET_NAMES[$mnt]}"
        local df_line=$(df -h "$mnt" 2>/dev/null | tail -1)
        local used=$(echo "$df_line" | awk '{print $3}')
        local avail=$(echo "$df_line" | awk '{print $4}')
        local pct=$(echo "$df_line" | awk '{print $5}')
        printf "  %-24s %-10s %-10s %s\n" "$name" "$used" "$avail" "$pct"
    done
}

generate_growth_section() {
    for mnt in "$MOUNT_BACKUP" "$MOUNT_BACKUP_SYSTEM" "$MOUNT_BACKUP_SYSTEM_MIRROR"; do
        if ! mountpoint -q "$mnt" 2>/dev/null; then continue; fi
        local name="${TARGET_NAMES[$mnt]}"
        local current="${USAGE_AFTER[$mnt]:-0}"
        local today_delta=$(( ${USAGE_AFTER[$mnt]:-0} - ${USAGE_BEFORE[$mnt]:-0} ))

        printf "  %s:\n" "$name"
        printf "    Today:              +%s\n" "$(format_bytes $today_delta)"

        local avg_7d=$(compute_growth_stats "$mnt" "$current" 7)
        local avg_30d=$(compute_growth_stats "$mnt" "$current" 30)

        if (( avg_7d > 0 )); then
            printf "    7-day avg:          %s/day\n" "$(format_bytes $avg_7d)"
        fi
        if (( avg_30d > 0 )); then
            printf "    30-day avg:         %s/day\n" "$(format_bytes $avg_30d)"
        fi

        # Capacity runway projection
        local avail_bytes=$(df --output=avail -B1 "$mnt" 2>/dev/null | tail -1 | tr -d ' ')
        local growth_rate=$avg_30d
        if (( growth_rate <= 0 )); then growth_rate=$avg_7d; fi
        if (( growth_rate <= 0 && today_delta > 0 )); then growth_rate=$today_delta; fi

        if (( growth_rate > 0 && avail_bytes > 0 )); then
            local days_left=$(( avail_bytes / growth_rate ))
            local years_left=$(printf "%.1f" "$(( days_left / 365.0 ))")
            printf "    Capacity runway:    ~%s days (~%s years)\n" "$days_left" "$years_left"
        else
            printf "    Capacity runway:    no growth trend yet\n"
        fi
        echo ""
    done
}

generate_smart_section() {
    local -A bay_names=(
        ["$DRIVE_PRIMARY"]="Bay 2  22TB"
        ["$DRIVE_SYSTEM_2TB"]="Bay 6  2TB "
        ["$DRIVE_SYSTEM_MIRROR"]="Bay 1  2TB "
    )

    for drive in "$DRIVE_PRIMARY" "$DRIVE_SYSTEM_2TB" "$DRIVE_SYSTEM_MIRROR"; do
        if [[ -z "$drive" || ! -b "$drive" ]]; then continue; fi
        local bay="${bay_names[$drive]}"
        local smart_data=$(get_smart_summary "$drive")
        local health=${smart_data%%|*}; smart_data=${smart_data#*|}
        local temp=${smart_data%%|*}; smart_data=${smart_data#*|}
        local hours=${smart_data%%|*}; smart_data=${smart_data#*|}
        local serial=$smart_data
        printf "  %-10s %-10s %-8s %s°C  %s hours\n" "$bay" "$serial" "$health" "$temp" "$hours"
    done
}

send_report() {
    local report="$1"
    local overall_status="$2"

    # Always save to file for reference
    mkdir -p "$(dirname "$LAST_REPORT")"
    echo "$report" > "$LAST_REPORT"
    log_info "Report saved to $LAST_REPORT"

    # Load email config
    if [[ ! -f "$EMAIL_CONF" ]]; then
        log_warn "Email config $EMAIL_CONF not found — report saved but not emailed"
        return 1
    fi

    source "$EMAIL_CONF"

    if [[ -z "${REPORT_TO:-}" ]]; then
        log_warn "REPORT_TO not set in $EMAIL_CONF — report saved but not emailed"
        return 1
    fi

    if [[ -z "${SMTP_AUTH_PASS:-}" ]]; then
        log_warn "SMTP_AUTH_PASS not set in $EMAIL_CONF — report saved but not emailed"
        log_warn "Get the Bridge password from: Proton Bridge GUI → Account → IMAP/SMTP → Password"
        return 1
    fi

    # Build subject line
    local subject="[DAS Backup] $(hostname) — $overall_status — $(date '+%Y-%m-%d %H:%M')"

    # Send via s-nail (mailx) with Proton Bridge SMTP
    # stderr suppressed to hide s-nail v14 deprecation warnings
    echo "$report" | mailx \
        -s "$subject" \
        -r "${REPORT_FROM:-das-backup@$(hostname)}" \
        -S "smtp=${SMTP_URL}" \
        -S "smtp-auth=login" \
        -S "smtp-auth-user=${SMTP_AUTH_USER}" \
        -S "smtp-auth-password=${SMTP_AUTH_PASS}" \
        -S "ssl-verify=${SMTP_SSL_VERIFY:-strict}" \
        "$REPORT_TO" 2>/dev/null && {
        log_info "Report emailed to $REPORT_TO"
        return 0
    } || {
        log_warn "Failed to email report to $REPORT_TO — saved to $LAST_REPORT"
        return 1
    }
}

# ============================================================================
# CLEANUP
# ============================================================================

cleanup() {
    log_warn "Cleaning up after error..."
    unmount_all
}

# ============================================================================
# MAIN
# ============================================================================

main() {
    local mode="run"
    local force_full="false"

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --dryrun|-n)
                mode="dryrun"
                ;;
            --full|-f)
                force_full="true"
                ;;
            *)
                echo "Usage: $0 [--dryrun|-n] [--full|-f]"
                echo "  --dryrun  Preview backup without making changes"
                echo "  --full    Force recreation of boot subvolumes"
                exit 1
                ;;
        esac
        shift
    done

    # Ensure log directory exists
    mkdir -p "$(dirname "$LOG_FILE")"

    echo "========================================"
    echo "  DAS Backup (22TB Primary)"
    echo "  Mode: $mode"
    echo "  Date: $(date '+%Y-%m-%d %H:%M:%S')"
    echo "========================================"
    echo ""

    log_info "=== DAS Backup Started ==="

    trap cleanup ERR

    check_root
    check_das_connected
    set_io_scheduler
    create_mount_points
    mount_sources
    create_snapshot_dirs
    mount_targets
    create_target_dirs

    # Populate target display names (used by throughput and report functions)
    TARGET_NAMES=(
        ["$MOUNT_BACKUP"]="22TB Primary (Bay 2)"
        ["$MOUNT_BACKUP_SYSTEM"]="2TB System (Bay 6)"
        ["$MOUNT_BACKUP_SYSTEM_MIRROR"]="2TB Mirror (Bay 1)"
    )

    if [[ "$mode" != "dryrun" ]]; then
        capture_usage "before"
    fi

    run_btrbk "$mode"

    if [[ "$mode" != "dryrun" ]]; then
        capture_usage "after"
        log_throughput
        update_boot_subvolumes "$force_full"
        sync_das_esp
        show_stats

        # Record growth data and generate email report
        record_growth

        local overall_status="SUCCESS"
        for op in "${(@k)OP_STATUS}"; do
            if [[ "${OP_STATUS[$op]}" == "FAIL" ]]; then
                overall_status="FAILURE"
                break
            fi
        done

        local report=$(generate_report)
        echo ""
        echo "$report"
        send_report "$report" "$overall_status"
    fi

    unmount_all

    log_info "=== DAS Backup Completed ==="
    echo ""
    log_info "Backup complete. DAS can be safely disconnected."
}

main "$@"
