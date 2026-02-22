#!/usr/bin/env zsh
# backup-run.sh - Run btrbk backup to DAS drives (config-driven)
# Version: 4.0.0
# Date: 2026-02-21
#
# Features:
#   - Incremental BTRFS backups via btrbk to configured targets
#   - Maintains stable boot subvolumes (@ and @home) for disaster recovery
#   - Syncs ESP to bootable recovery drives (config-driven)
#   - Detects DAS drives by serial number (stable across reboots)
#   - Logs per-target throughput (data written + MB/s rate)
#   - Designed for unattended nightly execution
#   - All configuration loaded from config.toml via btrdasd
#
# Prerequisites:
#   - DAS connected and powered on
#   - btrdasd built and installed
#   - config.toml installed at /etc/das-backup/config.toml
#
# Usage:
#   sudo ./backup-run.sh              # Incremental backup
#   sudo ./backup-run.sh --dryrun     # Preview only
#   sudo ./backup-run.sh --full       # Force full backup (recreate boot subvols)

set -euo pipefail
setopt typeset_silent  # prevent local/typeset from printing on re-declare in loops
zmodload zsh/datetime  # provides $EPOCHSECONDS for throughput timing

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

# Build associative arrays from config
declare -A DAS_SERIALS=()
declare -A TARGET_MOUNTS=()
declare -A TARGET_NAMES=()
declare -A TARGET_ROLES=()
for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
    label_var="DAS_TARGET_${i}_LABEL"
    serial_var="DAS_TARGET_${i}_SERIAL"
    mount_var="DAS_TARGET_${i}_MOUNT"
    name_var="DAS_TARGET_${i}_DISPLAY_NAME"
    role_var="DAS_TARGET_${i}_ROLE"
    DAS_SERIALS[${(P)label_var}]="${(P)serial_var}"
    TARGET_MOUNTS[${(P)label_var}]="${(P)mount_var}"
    TARGET_ROLES[${(P)label_var}]="${(P)role_var}"
    if [[ -n "${(P)name_var:-}" ]]; then
        TARGET_NAMES[${(P)mount_var}]="${(P)name_var}"
    else
        TARGET_NAMES[${(P)mount_var}]="${(P)label_var}"
    fi
done

# Source volumes and devices from config
declare -A SOURCE_VOLUMES=()
declare -A SOURCE_DEVICES=()
declare -A SOURCE_SNAPSHOT_DIRS=()
for (( i=0; i<DAS_SOURCE_COUNT; i++ )); do
    label_var="DAS_SOURCE_${i}_LABEL"
    vol_var="DAS_SOURCE_${i}_VOLUME"
    dev_var="DAS_SOURCE_${i}_DEVICE"
    snap_var="DAS_SOURCE_${i}_SNAPSHOT_DIR"
    SOURCE_VOLUMES[${(P)label_var}]="${(P)vol_var}"
    SOURCE_DEVICES[${(P)label_var}]="${(P)dev_var}"
    if [[ -n "${(P)snap_var:-}" ]]; then
        SOURCE_SNAPSHOT_DIRS[${(P)label_var}]="${(P)snap_var}"
    fi
done

# Logging (now from config)
LOG_FILE="$DAS_LOG_FILE"

# Email and growth tracking (now from config)
EMAIL_CONF="/etc/das-backup-email.conf"
GROWTH_LOG="$DAS_GROWTH_LOG"
LAST_REPORT="$DAS_LAST_REPORT"

# ESP mount points from config
MOUNT_DAS_ESP=(${(s: :)DAS_ESP_MOUNT_POINTS})

# All target mount points (space-separated string from config -> array)
ALL_TARGET_MOUNTS=(${(s: :)DAS_ALL_TARGET_MOUNTS})

# Throughput tracking (populated at runtime)
declare -A USAGE_BEFORE=()
declare -A USAGE_AFTER=()
BTRBK_START_TIME=0
BTRBK_END_TIME=0

# Operation status tracking (for email report)
declare -A OP_STATUS=()

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

    # Detect all target drives by serial, storing discovered device paths
    declare -gA DISCOVERED_DEVICES=()
    local required_found=true

    for label in "${(@k)DAS_SERIALS}"; do
        local serial="${DAS_SERIALS[$label]}"
        local role="${TARGET_ROLES[$label]}"
        local dev
        dev=$(find_device_by_serial "$serial") || dev=""

        if [[ -n "$dev" ]]; then
            DISCOVERED_DEVICES[$label]="$dev"
            log_info "  $label: $dev ($serial) — $role"
        else
            if [[ "$role" == "primary" ]]; then
                log_error "Primary backup drive ($label, $serial) not found"
                log_error "Is the DAS connected and powered on?"
                required_found=false
            else
                log_warn "  $label ($serial) not found — will skip"
            fi
        fi
    done

    if [[ "$required_found" == "false" ]]; then
        exit 1
    fi
}

set_io_scheduler() {
    log_info "Setting I/O scheduler to $DAS_IO_SCHEDULER for DAS drives..."

    for label in "${(@k)DISCOVERED_DEVICES}"; do
        local drive="${DISCOVERED_DEVICES[$label]}"
        if [[ -n "$drive" && -b "$drive" ]]; then
            local dev="${drive#/dev/}"
            if [[ -f "/sys/block/$dev/queue/scheduler" ]]; then
                echo "$DAS_IO_SCHEDULER" > "/sys/block/$dev/queue/scheduler" 2>/dev/null || true
            fi
        fi
    done
}

create_mount_points() {
    log_info "Creating mount points..."
    for label in "${(@k)SOURCE_VOLUMES}"; do
        mkdir -p "${SOURCE_VOLUMES[$label]}"
    done
    for label in "${(@k)TARGET_MOUNTS}"; do
        mkdir -p "${TARGET_MOUNTS[$label]}"
    done
    for esp in "${MOUNT_DAS_ESP[@]}"; do
        mkdir -p "$esp"
    done
}

mount_sources() {
    log_info "Mounting source top-level volumes..."

    for label in "${(@k)SOURCE_VOLUMES}"; do
        local mnt="${SOURCE_VOLUMES[$label]}"
        local dev="${SOURCE_DEVICES[$label]}"
        if ! mountpoint -q "$mnt"; then
            mount -o subvolid=5 "$dev" "$mnt"
            log_info "  Mounted $label at $mnt"
        fi
    done
}

mount_targets() {
    log_info "Mounting backup targets..."

    for label in "${(@k)TARGET_MOUNTS}"; do
        local mnt="${TARGET_MOUNTS[$label]}"
        local dev="${DISCOVERED_DEVICES[$label]:-}"

        if [[ -z "$dev" ]]; then
            continue
        fi

        local role="${TARGET_ROLES[$label]}"

        if ! mountpoint -q "$mnt"; then
            # Determine the right partition suffix based on role
            local part_dev
            if [[ "$role" == "primary" ]]; then
                part_dev="${dev}1"  # Single partition, whole-disk BTRFS
            else
                part_dev="${dev}2"  # Partition 2 for bootable drives (partition 1 = ESP)
            fi

            if [[ ! -b "$part_dev" ]]; then
                log_warn "Partition $part_dev not found — skipping $label"
                continue
            fi

            if mount -o "$DAS_MOUNT_OPTS" "$part_dev" "$mnt"; then
                log_info "  Mounted $label at $mnt"
            else
                log_warn "  Could not mount $label at $mnt — btrbk will skip it"
            fi
        fi
    done
}

create_snapshot_dirs() {
    log_info "Creating btrbk snapshot directories..."
    for label in "${(@k)SOURCE_SNAPSHOT_DIRS}"; do
        local snap_dir="${SOURCE_SNAPSHOT_DIRS[$label]}"
        if [[ -n "$snap_dir" ]]; then
            mkdir -p "$snap_dir"
        fi
    done
}

create_target_dirs() {
    log_info "Creating target directory structure..."

    for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
        local mount_var="DAS_TARGET_${i}_MOUNT"
        local subdirs_var="DAS_TARGET_${i}_TARGET_SUBDIRS"
        local mnt="${(P)mount_var}"

        if ! mountpoint -q "$mnt" 2>/dev/null; then
            continue
        fi

        if [[ -n "${(P)subdirs_var:-}" ]]; then
            for subdir in ${(s: :)${(P)subdirs_var}}; do
                mkdir -p "$mnt/$subdir"
            done
        fi
    done
}

run_btrbk() {
    local mode="${1:-run}"

    log_info "Running btrbk ($mode)..."

    if [[ "$mode" == "dryrun" ]]; then
        btrbk -c "$DAS_BTRBK_CONF" dryrun
        record_op "btrbk" "OK" "dryrun"
    else
        if btrbk -c "$DAS_BTRBK_CONF" run; then
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
    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
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
    if [[ "$DAS_ESP_ENABLED" != "true" ]]; then
        log_info "ESP sync disabled in config — skipping"
        record_op "esp_sync" "SKIP" "disabled"
        return
    fi

    log_info "Syncing ESP to DAS backup drives..."

    local esp_source="/boot"
    local esp_idx=0
    local esp_ok=0 esp_fail=0

    # Build list of ESP partitions from config
    local esp_parts=(${(s: :)DAS_ESP_PARTITIONS})
    local esp_total=${#esp_parts[@]}

    for esp_part in "${esp_parts[@]}"; do
        local mount_point="${MOUNT_DAS_ESP[$((esp_idx + 1))]}"
        esp_idx=$((esp_idx + 1))

        if [[ ! -b "$esp_part" ]]; then
            log_warn "ESP partition $esp_part not found — skipping"
            (( esp_fail += 1 ))
            continue
        fi

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
    # Unmount targets in reverse order
    for (( i=${#ALL_TARGET_MOUNTS[@]}; i>=1; i-- )); do
        umount "${ALL_TARGET_MOUNTS[$i]}" 2>/dev/null || true
    done
    # Unmount sources
    for label in "${(@k)SOURCE_VOLUMES}"; do
        umount "${SOURCE_VOLUMES[$label]}" 2>/dev/null || true
    done

    log_info "All volumes unmounted"
}

show_stats() {
    log_info "Backup statistics:"
    btrbk -c "$DAS_BTRBK_CONF" list latest 2>/dev/null || true

    log_info "Target disk usage:"
    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        if mountpoint -q "$mnt" 2>/dev/null; then
            df -h "$mnt" 2>/dev/null || true
        fi
    done
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

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
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

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        local name="${TARGET_NAMES[$mnt]:-$mnt}"
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

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
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
# CONTENT INDEXER
# ============================================================================

run_indexer() {
    if [[ ! -x "$BTRDASD_BIN" ]]; then
        log_warn "Content indexer not built -- skipping (build with: cargo build --release --manifest-path indexer/Cargo.toml)"
        record_op "indexer" "SKIP" "binary not found"
        return
    fi

    # Find the primary target mount for indexing
    local primary_mount=""
    for label in "${(@k)TARGET_ROLES}"; do
        if [[ "${TARGET_ROLES[$label]}" == "primary" ]]; then
            primary_mount="${TARGET_MOUNTS[$label]}"
            break
        fi
    done

    if [[ -z "$primary_mount" ]] || ! mountpoint -q "$primary_mount" 2>/dev/null; then
        log_warn "Primary target not mounted — skipping indexer"
        record_op "indexer" "SKIP" "primary target not mounted"
        return
    fi

    log_info "Running content indexer..."
    local indexer_output
    if indexer_output=$("$BTRDASD_BIN" walk "$primary_mount" --db "$DAS_DB_PATH" 2>&1); then
        record_op "indexer" "OK"
        log_info "  $indexer_output"
    else
        log_warn "Content indexer failed (non-fatal)"
        record_op "indexer" "FAIL" "exit code $?"
    fi
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
  Content indexer        ${OP_STATUS[indexer]:-N/A}  (${OP_STATUS[indexer_detail]:-n/a})

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
$(btrbk -c "$DAS_BTRBK_CONF" list latest 2>/dev/null | awk 'NR>1{printf "  %s\n", $0}' || echo "  (none yet)")

===============================================================
  backup-run.sh v4.0.0
  Next scheduled: $(systemctl show das-backup.timer --property=NextElapseUSecRealtime 2>/dev/null | cut -d= -f2 | sed 's/ [A-Z]*$//' || echo "unknown")
===============================================================
REPORT
}

generate_throughput_section() {
    local elapsed=$(( BTRBK_END_TIME - BTRBK_START_TIME ))
    local total_written=0

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        local name="${TARGET_NAMES[$mnt]:-$mnt}"
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

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        if ! mountpoint -q "$mnt" 2>/dev/null; then continue; fi
        local name="${TARGET_NAMES[$mnt]:-$mnt}"
        local df_line=$(df -h "$mnt" 2>/dev/null | tail -1)
        local used=$(echo "$df_line" | awk '{print $3}')
        local avail=$(echo "$df_line" | awk '{print $4}')
        local pct=$(echo "$df_line" | awk '{print $5}')
        printf "  %-24s %-10s %-10s %s\n" "$name" "$used" "$avail" "$pct"
    done
}

generate_growth_section() {
    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        if ! mountpoint -q "$mnt" 2>/dev/null; then continue; fi
        local name="${TARGET_NAMES[$mnt]:-$mnt}"
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
    for label in "${(@k)DISCOVERED_DEVICES}"; do
        local drive="${DISCOVERED_DEVICES[$label]}"
        if [[ -z "$drive" || ! -b "$drive" ]]; then continue; fi
        local name="${TARGET_NAMES[${TARGET_MOUNTS[$label]}]:-$label}"
        local smart_data=$(get_smart_summary "$drive")
        local health=${smart_data%%|*}; smart_data=${smart_data#*|}
        local temp=${smart_data%%|*}; smart_data=${smart_data#*|}
        local hours=${smart_data%%|*}; smart_data=${smart_data#*|}
        local serial=$smart_data
        printf "  %-24s %-10s %-8s %s°C  %s hours\n" "$name" "$serial" "$health" "$temp" "$hours"
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
    echo "  DAS Backup (config-driven)"
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
        run_indexer

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
