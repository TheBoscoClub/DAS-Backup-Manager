#!/bin/bash
# backup-run.sh - Run btrbk backup to DAS drives (config-driven)
# Version: 4.2.4
# Date: 2026-08-01
#
# Features:
#   - Boot archive-then-recreate + pruner wiring (v4.2.4): update_boot_subvolumes()
#     now archives the outgoing @/@home as a read-only snapshot
#     (@.archive.<TS> / @home.archive.<TS>, TS=YYYYMMDDTHHMMSS) BEFORE the
#     create-then-swap replacement on --full runs, matching indexer/src/backup.rs
#     archive_boot() semantics exactly. If the archive snapshot fails, the
#     recreation is skipped for that subvolume so the only copy is never
#     destroyed. boot-archive-cleanup.sh (previously installed but never
#     invoked) is now run at the end of every backup, daily and full alike,
#     while targets are still mounted, and reports through
#     record_op "archive_cleanup". Tracks DAS-Backup-Manager-64h, -1j7.
#   - Incremental BTRFS backups via btrbk to configured targets
#   - Maintains stable boot subvolumes (@ and @home) for disaster recovery
#   - Detects DAS drives by serial number (stable across reboots)
#   - RAID-1 multi-serial + mount-by-UUID: surviving-leg backups continue when
#     a single RAID-1 partner is missing (degraded mount), no manual config
#     edit required. See DAS-Backup-Manager-2qe.
#   - Bare-mountpoint guard (v4.2.2): refuses to invoke btrbk unless every
#     configured target either (a) is a real mountpoint backed by the
#     expected UUID/serial, or (b) does not exist on disk at all. Unmounted
#     bare directories at $mnt are auto-removed (when empty) so btrbk
#     cannot silently write to / instead of the DAS. Prevents recurrence of
#     the May 2026 incident where a removed 22TB drive let btrbk fill the
#     root filesystem.
#   - Snapshot_dir absolute-path resolution (v4.2.3): create_snapshot_dirs
#     now composes ${SOURCE_VOLUMES[label]}/${SOURCE_SNAPSHOT_DIRS[label]}
#     before mkdir, so newly-added sources whose snapshot_dir is relative
#     (the common case in config.toml) land on the source's volume rather
#     than systemd's cwd (/). Fixes the silent-skip pattern where btrbk
#     would emit "Failed to fetch subvolume detail for snapshot_dir" for
#     7 consecutive nightly runs after the 2026-05-17 commit added the
#     hdd-media and hdd-system sources. Tracks DAS-Backup-Manager-0p2.
#   - Logs per-target throughput (data written + MB/s rate)
#   - Designed for unattended nightly execution
#   - All configuration loaded from config.toml via btrdasd
#   - Single-instance guard via flock on /run/das-backup.lock — second
#     invocation exits 0 immediately if another is running. Protects against
#     timer fires during long runs, Sentinel auto-restart races, and direct
#     manual invocations.
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

# ============================================================================
# SINGLE-INSTANCE GUARD
# ============================================================================
# Refuse to run a second backup if another das-backup invocation is already in
# progress. /run is tmpfs (auto-cleared at boot) so the lockfile can't go stale
# across reboots. Exit 0 (not failure) when locked so that cachyos-sentinel
# does not interpret a skipped concurrent fire as a unit failure needing retry.
LOCKFILE="/run/das-backup.lock"
exec 9>"$LOCKFILE"
if ! flock -n 9; then
    echo "[INFO] Another das-backup run holds $LOCKFILE — skipping this invocation" >&2
    exit 0
fi
# FD 9 stays open for the rest of the script; lock auto-releases when the
# process exits (FD 9 closes), no explicit unlock needed.

# ============================================================================
# CONFIGURATION (loaded from config.toml via btrdasd)
# ============================================================================

# Load configuration from config.toml via btrdasd
BTRDASD_BIN="${BTRDASD_BIN:-/usr/bin/btrdasd}"
DAS_CONFIG="${DAS_CONFIG:-/etc/das-backup/config.toml}"
# Boot archive pruner — sibling script, same install directory as this one.
BOOT_ARCHIVE_CLEANUP_BIN="${BOOT_ARCHIVE_CLEANUP_BIN:-/usr/lib/das-backup/boot-archive-cleanup.sh}"
if [[ -x "$BTRDASD_BIN" ]]; then
    eval "$("$BTRDASD_BIN" config dump-env --config "$DAS_CONFIG")"
else
    echo "ERROR: btrdasd not found at $BTRDASD_BIN" >&2
    exit 1
fi

# Build associative arrays from config
declare -A DAS_SERIALS=()           # legacy: anchor serial per label
declare -A DAS_SERIALS_LIST=()      # space-separated list of all expected serials
declare -A TARGET_MOUNT_UUIDS=()    # BTRFS FS UUID for mount-by-UUID, "" if unset
declare -A TARGET_MOUNTS=()
declare -A TARGET_NAMES=()
declare -A TARGET_ROLES=()
declare -A MOUNT_ROLES=()
for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
    label_var="DAS_TARGET_${i}_LABEL"
    serial_var="DAS_TARGET_${i}_SERIAL"
    serials_var="DAS_TARGET_${i}_SERIALS"
    uuid_var="DAS_TARGET_${i}_MOUNT_UUID"
    mount_var="DAS_TARGET_${i}_MOUNT"
    name_var="DAS_TARGET_${i}_DISPLAY_NAME"
    role_var="DAS_TARGET_${i}_ROLE"
    label="${!label_var}"
    DAS_SERIALS[$label]="${!serial_var}"
    # SERIALS list (new env var) — fall back to legacy single SERIAL when not emitted
    DAS_SERIALS_LIST[$label]="${!serials_var:-${!serial_var}}"
    TARGET_MOUNT_UUIDS[$label]="${!uuid_var:-}"
    TARGET_MOUNTS[$label]="${!mount_var}"
    TARGET_ROLES[$label]="${!role_var}"
    MOUNT_ROLES[${!mount_var}]="${!role_var}"
    name_val="${!name_var}"
    if [[ -n "${name_val:-}" ]]; then
        TARGET_NAMES[${!mount_var}]="$name_val"
    else
        TARGET_NAMES[${!mount_var}]="$label"
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
    SOURCE_VOLUMES[${!label_var}]="${!vol_var}"
    SOURCE_DEVICES[${!label_var}]="${!dev_var}"
    snap_val="${!snap_var}"
    if [[ -n "${snap_val:-}" ]]; then
        SOURCE_SNAPSHOT_DIRS[${!label_var}]="$snap_val"
    fi
done

# Logging (now from config)
LOG_FILE="$DAS_LOG_FILE"

# Email and growth tracking (now from config)
# Bridge credentials live in ~/.config/pbridge.conf — the single canonical source for
# Protonmail Bridge SMTP credentials across all projects. See ~/.claude/rules/infrastructure.md.
# Path is hardcoded because backup-run.sh runs as root via systemd (no $HOME).
PBRIDGE_CONF="${PBRIDGE_CONF:-/home/bosco/.config/pbridge.conf}"
GROWTH_LOG="$DAS_GROWTH_LOG"
LAST_REPORT="$DAS_LAST_REPORT"

# All target mount points (space-separated string from config -> array)
IFS=' ' read -ra ALL_TARGET_MOUNTS <<< "$DAS_ALL_TARGET_MOUNTS"

# Throughput tracking (populated at runtime)
declare -A USAGE_BEFORE=()
declare -A USAGE_AFTER=()
BTRBK_START_TIME=0
BTRBK_END_TIME=0

# Operation status tracking (for email report)
declare -A OP_STATUS=()

# Track whether backup run has been recorded (prevents double-recording)
BACKUP_RUN_RECORDED="false"
# Track whether we're in a real (non-dryrun) backup
BACKUP_MODE_REAL="false"
# Track force_full for cleanup trap access
BACKUP_FORCE_FULL="false"

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
    local timestamp
    timestamp=$(date '+%Y-%m-%d %H:%M:%S')
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
    local dev dev_serial
    for dev in /dev/sd[a-z] /dev/sd[a-z][a-z]; do
        if [[ -b "$dev" ]]; then
            dev_serial=$(smartctl -i "$dev" 2>/dev/null | awk '/Serial Number:/{print $3}')
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

    # Each target may have one or more expected serials (RAID-1 → 2). For each
    # target we record the *first* device found in DISCOVERED_DEVICES (used by
    # legacy code paths) and the *availability* in TARGET_AVAILABLE.
    #
    # A target is AVAILABLE when:
    #   - mount_uuid is set, AND any expected serial is present OR the FS UUID
    #     itself is discoverable on the system (BTRFS multi-device superblock
    #     can satisfy this from the surviving leg alone)
    #   - or, mount_uuid is unset (legacy targets) AND at least one expected
    #     serial is present
    #
    # An UNAVAILABLE primary target aborts the run; an unavailable mirror is
    # skipped with a warning. RAID-1 partial-membership is degraded, not an
    # outage — we proceed with a warning.
    declare -gA DISCOVERED_DEVICES=()
    declare -gA TARGET_AVAILABLE=()
    local any_primary_available="false"
    local any_primary_configured="false"

    for label in "${!DAS_SERIALS[@]}"; do
        local serials_list="${DAS_SERIALS_LIST[$label]}"
        local uuid="${TARGET_MOUNT_UUIDS[$label]}"
        local role="${TARGET_ROLES[$label]}"
        if [[ "$role" == "primary" ]]; then
            any_primary_configured="true"
        fi

        local first_dev=""
        local -a found_devs=()
        local -a missing_serials=()
        local -a found_serials=()
        local serial
        for serial in $serials_list; do
            local dev
            dev=$(find_device_by_serial "$serial") || dev=""
            if [[ -n "$dev" ]]; then
                found_devs+=("$dev")
                found_serials+=("$serial")
                if [[ -z "$first_dev" ]]; then
                    first_dev="$dev"
                fi
            else
                missing_serials+=("$serial")
            fi
        done

        local available="false"
        # Path 1: mount_uuid known → BTRFS can satisfy from any one leg
        if [[ -n "$uuid" ]]; then
            if (( ${#found_devs[@]} > 0 )); then
                available="true"
            elif blkid -U "$uuid" >/dev/null 2>&1; then
                # FS UUID resolves on this host even without a configured serial
                # match (e.g. a replacement drive with a yet-unknown serial).
                available="true"
                first_dev="$(blkid -U "$uuid" 2>/dev/null | sed 's/[0-9]*$//')"
            fi
        else
            # Path 2: legacy device-by-serial only
            if (( ${#found_devs[@]} > 0 )); then
                available="true"
            fi
        fi

        TARGET_AVAILABLE[$label]="$available"
        if [[ "$available" == "true" ]]; then
            DISCOVERED_DEVICES[$label]="$first_dev"
            if (( ${#missing_serials[@]} > 0 )); then
                log_warn "  $label: present=[${found_serials[*]}] missing=[${missing_serials[*]}] — RAID-1 degraded, proceeding"
            else
                log_info "  $label: $first_dev (${found_serials[*]:-<uuid-only>}) — $role"
            fi
            if [[ "$role" == "primary" ]]; then
                any_primary_available="true"
            fi
        else
            if [[ "$role" == "primary" ]]; then
                log_error "  $label ($role): no expected drives present and no mountable UUID"
                log_error "    expected serials: ${serials_list:-<none>}"
                log_error "    mount UUID:       ${uuid:-<unset>}"
            else
                log_warn "  $label ($role): no expected drives present — will skip"
            fi
        fi
    done

    # Only abort when at least one primary was configured AND none are reachable.
    if [[ "$any_primary_configured" == "true" && "$any_primary_available" != "true" ]]; then
        log_error "No primary backup target is available — aborting"
        log_error "Is the DAS connected and powered on?"
        exit 1
    fi
}

set_io_scheduler() {
    log_info "Setting I/O scheduler to $DAS_IO_SCHEDULER for DAS drives..."

    for label in "${!DISCOVERED_DEVICES[@]}"; do
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
    for label in "${!SOURCE_VOLUMES[@]}"; do
        mkdir -p "${SOURCE_VOLUMES[$label]}"
    done

    # Only create mountpoint dirs for targets we actually plan to mount.
    # Bare directories at $mnt are a foot-gun: btrbk's config references
    # them, and if the underlying DAS filesystem isn't mounted there at
    # backup time, btrbk will silently write to the bare directory on /
    # (the NVMe root), filling the root filesystem. This happened in May
    # 2026 when the original 22TB drive was removed and a backup ran
    # before the replacement was in place — see bd DAS-Backup-Manager-9on.
    #
    # For UNAVAILABLE targets, proactively rmdir any pre-existing empty
    # mountpoint dir so btrbk's path is non-existent at write time
    # (btrbk fails fast and safely). If the dir is non-empty, that's
    # evidence a prior write hit the bare dir — refuse to proceed.
    for label in "${!TARGET_MOUNTS[@]}"; do
        local mnt="${TARGET_MOUNTS[$label]}"
        local available="${TARGET_AVAILABLE[$label]:-false}"
        if [[ "$available" == "true" ]]; then
            mkdir -p "$mnt"
            continue
        fi

        if [[ ! -e "$mnt" ]]; then
            continue   # already non-existent — safe, nothing to do
        fi

        # Already mounted from a prior run? Try to unmount cleanly first.
        # (Should not happen given mount_targets symmetry, but be defensive.)
        if mountpoint -q "$mnt" 2>/dev/null; then
            log_warn "  $label is unavailable but $mnt is currently mounted — attempting umount"
            if ! umount "$mnt" 2>/dev/null; then
                log_error "  $label: refusing to proceed — $mnt is mounted but target is marked unavailable"
                exit 1
            fi
        fi

        # Now $mnt should be a bare directory. Remove it if empty.
        if rmdir "$mnt" 2>/dev/null; then
            log_info "  Removed pre-existing bare mountpoint $mnt (target unavailable)"
        else
            log_error "  ABORTING: target $label is unavailable but $mnt is non-empty."
            log_error "  This is the failure pattern that filled / in May 2026: a prior backup"
            log_error "  wrote data to a bare directory because the DAS target wasn't mounted."
            log_error "  Inspect $mnt manually, move/delete its contents (likely on /), and re-run."
            log_error "  See bd DAS-Backup-Manager-9on."
            exit 1
        fi
    done
}

mount_sources() {
    log_info "Mounting source top-level volumes..."

    for label in "${!SOURCE_VOLUMES[@]}"; do
        local mnt="${SOURCE_VOLUMES[$label]}"
        local dev="${SOURCE_DEVICES[$label]}"
        if ! mountpoint -q "$mnt"; then
            if [[ "$dev" == UUID=* ]]; then
                # UUID-based mount (stable across device letter changes)
                mount -t btrfs -o subvolid=5 "$dev" "$mnt"
            else
                mount -o subvolid=5 "$dev" "$mnt"
            fi
            log_info "  Mounted $label at $mnt"
        fi
    done
}

mount_targets() {
    log_info "Mounting backup targets..."

    for label in "${!TARGET_MOUNTS[@]}"; do
        local mnt="${TARGET_MOUNTS[$label]}"
        local available="${TARGET_AVAILABLE[$label]:-false}"
        local uuid="${TARGET_MOUNT_UUIDS[$label]}"
        local role="${TARGET_ROLES[$label]}"

        if [[ "$available" != "true" ]]; then
            continue
        fi

        if mountpoint -q "$mnt"; then
            continue
        fi

        # Prefer mount-by-UUID when configured. BTRFS auto-discovers all
        # present members from any single one's superblock, so a degraded
        # RAID-1 (one leg missing) still mounts cleanly. The DAS_MOUNT_OPTS
        # config value is expected to include `degraded` for RAID-1 targets.
        if [[ -n "$uuid" ]]; then
            if mount -t btrfs -o "$DAS_MOUNT_OPTS" "UUID=$uuid" "$mnt"; then
                log_info "  Mounted $label at $mnt (UUID=$uuid)"
                continue
            else
                log_warn "  UUID=$uuid mount failed for $label — falling back to device-based mount"
            fi
        fi

        # Legacy device-by-serial path (used when mount_uuid is unset)
        local dev="${DISCOVERED_DEVICES[$label]:-}"
        if [[ -z "$dev" ]]; then
            log_warn "  No discovered device for $label and no UUID configured — skipping"
            continue
        fi

        local part_dev
        if [[ "$role" == "primary" ]]; then
            part_dev="${dev}1"  # Single partition, whole-disk BTRFS
        else
            part_dev="${dev}2"  # Partition 2 for bootable drives (partition 1 = ESP)
        fi

        if [[ ! -b "$part_dev" ]]; then
            log_warn "  Partition $part_dev not found — skipping $label"
            continue
        fi

        if mount -o "$DAS_MOUNT_OPTS" "$part_dev" "$mnt"; then
            log_info "  Mounted $label at $mnt ($part_dev)"
        else
            log_warn "  Could not mount $label at $mnt — btrbk will skip it"
        fi
    done
}

create_snapshot_dirs() {
    # Each [[source]] in config.toml declares snapshot_dir relative to its
    # volume root. Resolve to an absolute path before mkdir so the directory
    # lands inside the source's volume rather than the script's cwd
    # (systemd-started services run with cwd=/, where mkdir would silently
    # create the dir on the root filesystem — see DAS-Backup-Manager-0p2).
    log_info "Creating btrbk snapshot directories..."
    for label in "${!SOURCE_SNAPSHOT_DIRS[@]}"; do
        local snap_dir="${SOURCE_SNAPSHOT_DIRS[$label]}"
        if [[ -z "$snap_dir" ]]; then
            continue
        fi

        local abs_dir
        if [[ "$snap_dir" = /* ]]; then
            abs_dir="$snap_dir"
        else
            local volume="${SOURCE_VOLUMES[$label]:-}"
            if [[ -z "$volume" ]]; then
                log_warn "  $label: no source volume known, cannot resolve snapshot_dir '$snap_dir' — skipping"
                continue
            fi
            abs_dir="${volume%/}/$snap_dir"
        fi

        if ! mkdir -p "$abs_dir" 2>/dev/null; then
            log_warn "  $label: failed to create snapshot_dir $abs_dir (mkdir error)"
            continue
        fi
        log_info "  $label: $abs_dir"
    done
}

create_target_dirs() {
    log_info "Creating target directory structure..."

    # Collect all target subdirs from sources
    local -a all_subdirs=()
    for (( i=0; i<DAS_SOURCE_COUNT; i++ )); do
        local subdirs_var="DAS_SOURCE_${i}_TARGET_SUBDIRS"
        local subdirs="${!subdirs_var:-}"
        if [[ -n "$subdirs" ]]; then
            IFS=' ' read -ra parts <<< "$subdirs"
            all_subdirs+=("${parts[@]}")
        fi
    done

    # Create subdirs on every mounted target
    for (( i=0; i<DAS_TARGET_COUNT; i++ )); do
        local mount_var="DAS_TARGET_${i}_MOUNT"
        local mnt="${!mount_var}"

        if ! mountpoint -q "$mnt" 2>/dev/null; then
            continue
        fi

        for subdir in "${all_subdirs[@]}"; do
            mkdir -p "$mnt/$subdir"
        done
    done
}

# Verify EVERY configured target is in one of two safe states before invoking
# btrbk:
#   1. Real mountpoint backed by the expected filesystem (UUID match preferred,
#      device-serial match as fallback). btrbk will write to the DAS as
#      intended.
#   2. Non-existent on disk (no directory at $mnt). btrbk will fail safely
#      with a missing-path error for that target rather than writing to /.
#
# Anything else — a bare directory at $mnt, or a mountpoint backed by an
# UNEXPECTED filesystem (wrong UUID, wrong serial, root filesystem
# shadowing the path) — is the May 2026 foot-gun and we ABORT.
#
# This is the last gate before btrbk gets to make irreversible decisions.
# Tracks bd DAS-Backup-Manager-9on.
verify_targets_before_btrbk() {
    log_info "Verifying every backup target is backed by an expected DAS device..."

    local violations=()

    for label in "${!TARGET_MOUNTS[@]}"; do
        local mnt="${TARGET_MOUNTS[$label]}"
        local available="${TARGET_AVAILABLE[$label]:-false}"
        local uuid="${TARGET_MOUNT_UUIDS[$label]}"
        local serials_list="${DAS_SERIALS_LIST[$label]}"
        local role="${TARGET_ROLES[$label]}"

        # State A: target intentionally not active this run (no DAS drive
        # present, mirror role). Confirmed-safe ONLY if $mnt does not exist;
        # otherwise btrbk could still write to a bare dir.
        if [[ "$available" != "true" ]]; then
            if [[ -e "$mnt" ]]; then
                violations+=("$label: marked unavailable but $mnt still exists (would let btrbk write to bare dir on /)")
            fi
            continue
        fi

        # State B: target should be active — MUST be a real mountpoint.
        if ! mountpoint -q "$mnt" 2>/dev/null; then
            violations+=("$label ($role): expected mounted but $mnt is NOT a mountpoint (mount failed silently in mount_targets)")
            continue
        fi

        # State B verification: the mounted filesystem must be the one we
        # expected, not some other filesystem (or worse, the root FS shadowing
        # an unmounted path). Prefer UUID match (works for BTRFS RAID-1 even
        # under degraded mount), fall back to disk serial.
        if [[ -n "$uuid" ]]; then
            local fs_uuid
            fs_uuid=$(findmnt -n -o UUID --target "$mnt" 2>/dev/null || true)
            if [[ "$fs_uuid" != "$uuid" ]]; then
                violations+=("$label: $mnt has fs UUID '${fs_uuid:-<none>}', expected '$uuid' — different filesystem mounted here")
                continue
            fi
            log_info "  $label: OK ($mnt → UUID=$fs_uuid)"
            continue
        fi

        # Legacy device-by-serial verification (used when mount_uuid is unset)
        local mount_src
        mount_src=$(findmnt -n -o SOURCE --target "$mnt" 2>/dev/null || true)
        if [[ -z "$mount_src" ]]; then
            violations+=("$label: $mnt is a mountpoint but findmnt could not resolve its source device")
            continue
        fi
        local mount_dev="${mount_src%%[0-9]*}"   # /dev/sde1 → /dev/sde
        local got_serial
        got_serial=$(smartctl -i "$mount_dev" 2>/dev/null | awk '/Serial Number:/{print $3}' || true)
        local serial_match="false"
        local expected
        for expected in $serials_list; do
            if [[ "$got_serial" == "$expected" ]]; then
                serial_match="true"
                break
            fi
        done
        if [[ "$serial_match" != "true" ]]; then
            violations+=("$label: $mnt mounted from $mount_src (serial='${got_serial:-?}'), expected one of: $serials_list")
            continue
        fi
        log_info "  $label: OK ($mnt → $mount_src, serial=$got_serial)"
    done

    if (( ${#violations[@]} > 0 )); then
        log_error "============================================================"
        log_error "ABORTING — refusing to invoke btrbk. Target verification failed:"
        log_error ""
        local v
        for v in "${violations[@]}"; do
            log_error "  - $v"
        done
        log_error ""
        log_error "Continuing would let btrbk write to a bare directory on the root"
        log_error "filesystem, filling /. This is the May 2026 incident pattern."
        log_error "See bd DAS-Backup-Manager-9on for the full failure-mode writeup."
        log_error "============================================================"
        record_op "verify_targets" "FAIL" "${#violations[@]} violation(s)"
        exit 1
    fi

    log_info "All backup targets verified — safe to invoke btrbk."
    record_op "verify_targets" "OK"
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
    # One timestamp per run (not per subvolume/target) — matches the Rust
    # archive_boot() format exactly so boot-archive-cleanup.sh's
    # parse_archive_timestamp() can parse either origin's archives.
    local ts
    ts=$(date +%Y%m%dT%H%M%S)

    log_info "Updating stable boot subvolumes..."

    # Update boot subvolumes on PRIMARY targets only — mirror targets are independent
    # bootable systems and must never have their @ replaced with host snapshots.
    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        if ! mountpoint -q "$mnt" 2>/dev/null; then
            continue
        fi

        # Skip mirror targets — they have their own OS installations
        local mount_role="${MOUNT_ROLES[$mnt]:-}"
        if [[ "$mount_role" == "mirror" ]]; then
            log_info "  [$(btrfs filesystem label "$mnt" 2>/dev/null || echo "$mnt")] Skipping mirror target (independent OS)"
            (( skipped += 1 ))
            continue
        fi

        local label
        label=$(btrfs filesystem label "$mnt" 2>/dev/null || echo "$mnt")
        # `|| true` guards against `set -o pipefail` + `set -e` killing the run when
        # grep finds zero matches — the empty-string check below is the intended path.
        local latest_root
        latest_root=$(btrfs subvolume list "$mnt" 2>/dev/null | grep "nvme/root\." | awk '{print $NF}' | sort | tail -1) || true
        local latest_home
        latest_home=$(btrfs subvolume list "$mnt" 2>/dev/null | grep "nvme/home\." | awk '{print $NF}' | sort | tail -1) || true

        if [[ -z "$latest_root" || -z "$latest_home" ]]; then
            log_warn "  [$label] No btrbk snapshots found, skipping"
            (( skipped += 1 ))
            continue
        fi

        log_info "  [$label] Latest root: $latest_root"
        log_info "  [$label] Latest home: $latest_home"

        # Update @ subvolume (archive-then-swap: archive the outgoing @ as a
        # read-only snapshot BEFORE the create-then-swap replacement, so the
        # only copy of the outgoing subvolume is never destroyed. If the
        # archive snapshot fails, skip the recreation entirely for this
        # subvolume — never delete @ without a preserved archive.)
        if btrfs subvolume show "$mnt/@" &>/dev/null; then
            if [[ "$force" == "true" ]]; then
                if ! btrfs subvolume snapshot -r "$mnt/@" "$mnt/@.archive.$ts"; then
                    log_error "  [$label] Failed to archive @ -> @.archive.$ts — skipping recreation (old @ preserved)"
                    (( failed += 1 ))
                elif btrfs subvolume snapshot "$mnt/$latest_root" "$mnt/@.new" && \
                     btrfs subvolume delete "$mnt/@" && \
                     mv "$mnt/@.new" "$mnt/@"; then
                    log_info "  [$label] Recreated @ from $latest_root (archived old @ -> @.archive.$ts)"
                    (( updated += 1 ))
                else
                    log_error "  [$label] Failed to recreate @ (archive @.archive.$ts was created)"
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

        # Update @home subvolume (archive-then-swap — see @ handling above for
        # the archive-before-delete rationale).
        if btrfs subvolume show "$mnt/@home" &>/dev/null; then
            if [[ "$force" == "true" ]]; then
                if ! btrfs subvolume snapshot -r "$mnt/@home" "$mnt/@home.archive.$ts"; then
                    log_error "  [$label] Failed to archive @home -> @home.archive.$ts — skipping recreation (old @home preserved)"
                    (( failed += 1 ))
                elif btrfs subvolume snapshot "$mnt/$latest_home" "$mnt/@home.new" && \
                     btrfs subvolume delete "$mnt/@home" && \
                     mv "$mnt/@home.new" "$mnt/@home"; then
                    log_info "  [$label] Recreated @home from $latest_home (archived old @home -> @home.archive.$ts)"
                    (( updated += 1 ))
                else
                    log_error "  [$label] Failed to recreate @home (archive @home.archive.$ts was created)"
                    (( failed += 1 ))
                fi
            else
                log_info "  [$label] @home exists, skipping (use --full to recreate)"
                (( skipped += 1 ))
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

# Prune expired boot-subvolume archives (@.archive.<TS> / @home.archive.<TS>)
# created by update_boot_subvolumes(). Must run while targets are still
# mounted — boot-archive-cleanup.sh silently skips any target mount point
# that isn't currently mounted. Soft-fail: a pruner failure is recorded for
# the email report but never aborts the backup, matching run_indexer().
run_archive_cleanup() {
    local mode="$1"
    local args=()

    if [[ "$mode" == "dryrun" ]]; then
        args+=(--dryrun)
    fi

    if [[ ! -x "$BOOT_ARCHIVE_CLEANUP_BIN" ]]; then
        log_warn "Boot archive cleanup script not found at $BOOT_ARCHIVE_CLEANUP_BIN — skipping"
        record_op "archive_cleanup" "FAIL" "script not found at $BOOT_ARCHIVE_CLEANUP_BIN"
        return
    fi

    log_info "Running boot archive cleanup..."
    local cleanup_output
    if cleanup_output=$("$BOOT_ARCHIVE_CLEANUP_BIN" "${args[@]}" 2>&1); then
        record_op "archive_cleanup" "OK" "$(echo "$cleanup_output" | tail -1)"
        log_info "Boot archive cleanup completed"
    else
        local exit_code=$?
        log_warn "Boot archive cleanup failed (non-fatal): $cleanup_output"
        record_op "archive_cleanup" "FAIL" "exit code $exit_code"
    fi
}

unmount_all() {
    log_info "Unmounting volumes..."

    # Unmount targets in reverse order (0-based indexing)
    for (( i=${#ALL_TARGET_MOUNTS[@]}-1; i>=0; i-- )); do
        umount "${ALL_TARGET_MOUNTS[$i]}" 2>/dev/null || true
    done
    # Unmount sources
    for label in "${!SOURCE_VOLUMES[@]}"; do
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
        awk "BEGIN {printf \"%.2f GiB\", $bytes / 1073741824}"
    elif (( bytes >= 1048576 )); then
        awk "BEGIN {printf \"%.2f MiB\", $bytes / 1048576}"
    elif (( bytes >= 1024 )); then
        awk "BEGIN {printf \"%.2f KiB\", $bytes / 1024}"
    else
        printf "%d B" "$bytes"
    fi
}

# Capture disk usage on all mounted backup targets
capture_usage() {
    local phase="$1"  # "before" or "after"

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        if mountpoint -q "$mnt" 2>/dev/null; then
            local used
            used=$(get_used_bytes "$mnt")
            if [[ "$phase" == "before" ]]; then
                USAGE_BEFORE[$mnt]=$used
            else
                USAGE_AFTER[$mnt]=$used
            fi
        fi
    done

    if [[ "$phase" == "before" ]]; then
        BTRBK_START_TIME=$(date +%s)
    else
        BTRBK_END_TIME=$(date +%s)
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
    local ts
    ts=$(date '+%Y-%m-%dT%H:%M:%S')

    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        if mountpoint -q "$mnt" 2>/dev/null; then
            local used
            used=$(get_used_bytes "$mnt")
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

    local now
    now=$(date +%s)
    local cutoff=$(( now - (lookback_days * 86400) ))

    # Find the oldest entry for this mount within the lookback window
    local oldest_epoch=0 oldest_used=0
    local ts entry_mnt entry_used entry_epoch
    while IFS=' ' read -r ts entry_mnt entry_used; do
        [[ "$entry_mnt" != "$mnt" ]] && continue
        # Parse ISO timestamp to epoch
        entry_epoch=$(date -d "$ts" '+%s' 2>/dev/null || echo 0)
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
    for label in "${!TARGET_ROLES[@]}"; do
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
        local exit_code=$?
        log_warn "Content indexer failed (non-fatal)"
        record_op "indexer" "FAIL" "exit code $exit_code"
    fi
}

# ============================================================================
# EMAIL REPORT
# ============================================================================

generate_report() {
    local hostname
    hostname=$(hostname)
    local timestamp
    timestamp=$(date '+%Y-%m-%d %H:%M')
    local overall_status="ALL OPERATIONS SUCCESSFUL"

    # Check for any failures
    for op in "${!OP_STATUS[@]}"; do
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
  Archive cleanup       ${OP_STATUS[archive_cleanup]:-N/A}  (${OP_STATUS[archive_cleanup_detail]:-n/a})
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
  backup-run.sh v4.2.4
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
        local df_line
        df_line=$(df -h "$mnt" 2>/dev/null | tail -1)
        local used avail pct
        used=$(echo "$df_line" | awk '{print $3}')
        avail=$(echo "$df_line" | awk '{print $4}')
        pct=$(echo "$df_line" | awk '{print $5}')
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

        local avg_7d avg_30d
        avg_7d=$(compute_growth_stats "$mnt" "$current" 7)
        avg_30d=$(compute_growth_stats "$mnt" "$current" 30)

        if (( avg_7d > 0 )); then
            printf "    7-day avg:          %s/day\n" "$(format_bytes "$avg_7d")"
        fi
        if (( avg_30d > 0 )); then
            printf "    30-day avg:         %s/day\n" "$(format_bytes "$avg_30d")"
        fi

        # Capacity runway projection
        local avail_bytes
        avail_bytes=$(df --output=avail -B1 "$mnt" 2>/dev/null | tail -1 | tr -d ' ')
        local growth_rate=$avg_30d
        if (( growth_rate <= 0 )); then growth_rate=$avg_7d; fi
        if (( growth_rate <= 0 && today_delta > 0 )); then growth_rate=$today_delta; fi

        if (( growth_rate > 0 && avail_bytes > 0 )); then
            local days_left=$(( avail_bytes / growth_rate ))
            local years_left
            years_left=$(awk "BEGIN {printf \"%.1f\", $days_left / 365}")
            printf "    Capacity runway:    ~%s days (~%s years)\n" "$days_left" "$years_left"
        else
            printf "    Capacity runway:    no growth trend yet\n"
        fi
        echo ""
    done
}

generate_smart_section() {
    for label in "${!DISCOVERED_DEVICES[@]}"; do
        local drive="${DISCOVERED_DEVICES[$label]}"
        if [[ -z "$drive" || ! -b "$drive" ]]; then continue; fi
        local name="${TARGET_NAMES[${TARGET_MOUNTS[$label]}]:-$label}"
        local smart_data
        smart_data=$(get_smart_summary "$drive")
        local health=${smart_data%%|*}; smart_data=${smart_data#*|}
        local temp=${smart_data%%|*}; smart_data=${smart_data#*|}
        local hours=${smart_data%%|*}; smart_data=${smart_data#*|}
        local serial=$smart_data
        printf "  %-24s %-10s %-8s %s°C  %s hours\n" "$name" "$serial" "$health" "$temp" "$hours"
    done
}

# Parse Bridge SMTP credentials from ~/.config/pbridge.conf (the single canonical
# source for Protonmail Bridge credentials per ~/.claude/rules/infrastructure.md).
# Sets globals: PB_SMTP_HOST, PB_SMTP_PORT, PB_SMTP_USER, PB_SMTP_PASS.
# Returns 0 on success, 1 if config missing or credentials incomplete.
parse_pbridge_smtp() {
    if [[ ! -f "$PBRIDGE_CONF" ]]; then
        log_warn "Bridge config $PBRIDGE_CONF not found — report saved but not emailed"
        log_warn "pbridge.conf is the single canonical Bridge credential source — see ~/.claude/rules/infrastructure.md"
        return 1
    fi

    # The token in this file is sensitive — warn loudly if perms aren't 0600
    local perms
    perms=$(stat -c '%a' "$PBRIDGE_CONF" 2>/dev/null || echo "???")
    if [[ "$perms" != "600" ]]; then
        log_warn "Bridge config $PBRIDGE_CONF has permissions $perms — should be 600 (contains auth token)"
    fi

    PB_SMTP_HOST=""; PB_SMTP_PORT=""; PB_SMTP_USER=""; PB_SMTP_PASS=""
    local in_smtp_block=0
    while IFS= read -r line; do
        if [[ "$line" =~ ^SMTP: ]]; then in_smtp_block=1; continue; fi
        if [[ "$line" =~ ^IMAP: ]]; then in_smtp_block=0; continue; fi
        if [[ "$line" =~ ^[A-Za-z][A-Za-z0-9]*:$ ]]; then in_smtp_block=0; continue; fi
        [[ $in_smtp_block -eq 0 ]] && continue
        if [[ "$line" =~ ^[[:space:]]*Hostname:[[:space:]]*(.+)$ ]]; then PB_SMTP_HOST="${BASH_REMATCH[1]}"; fi
        if [[ "$line" =~ ^[[:space:]]*Port:[[:space:]]*(.+)$ ]]; then PB_SMTP_PORT="${BASH_REMATCH[1]}"; fi
        if [[ "$line" =~ ^[[:space:]]*Username:[[:space:]]*(.+)$ ]]; then PB_SMTP_USER="${BASH_REMATCH[1]}"; fi
        if [[ "$line" =~ ^[[:space:]]*Password:[[:space:]]*(.+)$ ]]; then PB_SMTP_PASS="${BASH_REMATCH[1]}"; fi
    done < "$PBRIDGE_CONF"

    if [[ -z "$PB_SMTP_USER" || -z "$PB_SMTP_PASS" ]]; then
        log_warn "Bridge SMTP credentials missing in $PBRIDGE_CONF — report saved but not emailed"
        return 1
    fi
    return 0
}

send_report() {
    local report="$1"
    local overall_status="$2"

    # Always save to file for reference
    mkdir -p "$(dirname "$LAST_REPORT")"
    echo "$report" > "$LAST_REPORT"
    log_info "Report saved to $LAST_REPORT"

    # Source Bridge SMTP credentials from pbridge.conf (single canonical source)
    if ! parse_pbridge_smtp; then
        return 1
    fi

    local smtp_url="smtp://${PB_SMTP_HOST}:${PB_SMTP_PORT}"
    # Recipient defaults to the Bridge account holder (single-user setup).
    # Sender uses the Bridge account address (Bridge enforces SMTP MAIL FROM to match
    # the authenticated user) but renders a friendly display name "DAS Backup (<host>)"
    # so the email stands out in the inbox From column instead of looking like
    # gjbr@pm.me sending mail to itself. Override either via DAS_REPORT_TO/FROM.
    local report_to="${DAS_REPORT_TO:-$PB_SMTP_USER}"
    local report_from="${DAS_REPORT_FROM:-DAS Backup ($(hostname -s)) <$PB_SMTP_USER>}"

    # Build subject line
    local subject
    subject="[DAS Backup] $(hostname) — $overall_status — $(date '+%Y-%m-%d %H:%M')"

    # Send via s-nail (mailx) with Proton Bridge SMTP
    # stderr suppressed to hide s-nail v14 deprecation warnings
    # ssl-verify=ignore because Bridge uses a self-signed cert on the loopback listener
    if echo "$report" | mailx \
        -s "$subject" \
        -r "$report_from" \
        -S "smtp=${smtp_url}" \
        -S "smtp-auth=login" \
        -S "smtp-auth-user=${PB_SMTP_USER}" \
        -S "smtp-auth-password=${PB_SMTP_PASS}" \
        -S "ssl-verify=ignore" \
        "$report_to" 2>/dev/null; then
        log_info "Report emailed to $report_to"
        return 0
    else
        log_warn "Failed to email report to $report_to — saved to $LAST_REPORT"
        return 1
    fi
}

# ============================================================================
# RECORD BACKUP RUN IN DATABASE
# ============================================================================

record_backup_run_in_db() {
    # Guard against double-recording (normal path + cleanup trap)
    if [[ "$BACKUP_RUN_RECORDED" == "true" ]]; then
        return 0
    fi

    local overall_status="$1"
    local force_full="$2"

    local mode="incremental"
    if [[ "$force_full" == "true" ]]; then
        mode="full"
    fi

    local elapsed=$(( BTRBK_END_TIME - BTRBK_START_TIME ))

    # Compute total bytes sent across all targets
    local total_bytes=0
    for mnt in "${ALL_TARGET_MOUNTS[@]}"; do
        local before="${USAGE_BEFORE[$mnt]:-0}"
        local after="${USAGE_AFTER[$mnt]:-0}"
        local delta=$(( after - before ))
        if (( delta > 0 )); then
            total_bytes=$(( total_bytes + delta ))
        fi
    done

    # Count snapshots from btrbk
    local snaps_created=0
    local snaps_sent=0
    if btrbk_latest=$(btrbk -c "$DAS_BTRBK_CONF" list latest 2>/dev/null | grep -v "^SOURCE_SUBVOLUME"); then
        snaps_created=$(echo "$btrbk_latest" | grep -c "up-to-date" || true)
        # snaps_sent uses bytes written as a proxy: if data was transferred, snapshots were sent
        if (( total_bytes > 0 )); then
            snaps_sent=$snaps_created
        fi
    fi

    # Collect errors from failed operations (newline-separated for DB storage)
    local error_list=""
    for op in "${!OP_STATUS[@]}"; do
        if [[ "${OP_STATUS[$op]}" == "FAIL" ]]; then
            local detail="${OP_STATUS[${op}_detail]:-}"
            if [[ -n "$detail" ]]; then
                error_list="${error_list:+${error_list}$'\n'}${op}: ${detail}"
            else
                error_list="${error_list:+${error_list}$'\n'}${op} failed"
            fi
        fi
    done

    # Build args array to avoid quoting issues with empty values
    local args=(
        backup record-run
        --db "$DAS_DB_PATH"
        --mode "$mode"
        --snaps-created "$snaps_created"
        --snaps-sent "$snaps_sent"
        --bytes-sent "$total_bytes"
        --duration-secs "$elapsed"
    )
    if [[ "$overall_status" == "SUCCESS" ]]; then
        args+=(--success)
    fi
    if [[ -n "$error_list" ]]; then
        args+=(--errors "$error_list")
    fi

    local record_err
    if record_err=$("$BTRDASD_BIN" "${args[@]}" 2>&1); then
        log_info "Backup run recorded in database"
    else
        log_warn "Failed to record backup run in database: $record_err (non-fatal)"
    fi
    BACKUP_RUN_RECORDED="true"
}

# ============================================================================
# CLEANUP
# ============================================================================

cleanup() {
    log_warn "Cleaning up after error..."

    # Record the failed backup run if we were in a real backup and haven't recorded yet
    if [[ "$BACKUP_MODE_REAL" == "true" && "$BACKUP_RUN_RECORDED" == "false" ]]; then
        # Ensure end time is set (may not be if failure was during btrbk)
        if (( BTRBK_END_TIME == 0 )); then
            BTRBK_END_TIME=$(date +%s)
        fi
        record_backup_run_in_db "FAILURE" "$BACKUP_FORCE_FULL"
    fi

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
    verify_targets_before_btrbk

    if [[ "$mode" != "dryrun" ]]; then
        BACKUP_MODE_REAL="true"
        BACKUP_FORCE_FULL="$force_full"
        capture_usage "before"
    fi

    run_btrbk "$mode"

    if [[ "$mode" != "dryrun" ]]; then
        capture_usage "after"
        log_throughput
        update_boot_subvolumes "$force_full"
        show_stats
        run_indexer

        # Record growth data and generate email report
        record_growth

        # Prune expired boot archives (daily and full runs alike) while
        # targets are still mounted, before the report is built so pruner
        # failures are visible in overall_status and the email report.
        run_archive_cleanup "$mode"

        local overall_status="SUCCESS"
        for op in "${!OP_STATUS[@]}"; do
            if [[ "${OP_STATUS[$op]}" == "FAIL" ]]; then
                overall_status="FAILURE"
                break
            fi
        done

        local report
        report=$(generate_report)
        echo ""
        echo "$report"
        send_report "$report" "$overall_status" || \
            log_warn "Email delivery failed — backup itself completed successfully"

        # Record backup run in the database for GUI history
        record_backup_run_in_db "$overall_status" "$force_full"
    else
        # Dryrun mode never mutates boot subvolumes or sends an email report,
        # but the pruner still needs a preview pass (its own --dryrun) while
        # targets are mounted — boot-archive-cleanup.sh silently skips any
        # target that isn't currently mounted.
        run_archive_cleanup "$mode"
    fi

    unmount_all

    log_info "=== DAS Backup Completed ==="
    echo ""
    log_info "Backup complete. DAS can be safely disconnected."
}

main "$@"
