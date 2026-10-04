#!/bin/bash
# das-partition-drives.sh - Partition and format DAS backup drives (config-driven)
# Version: 2.3.0
# Date: 2026-10-04
#
# WARNING: This script DESTROYS ALL DATA on the target drives!
#     --run refuses unless every drive's most recent SMART self-test PASSED:
#     ATA "Completed without error" or SCSI "Completed", with smartctl's exit
#     status flagging nothing beyond bit 2 or 6 (smartctl(8)). A test still
#     running, failed, aborted, interrupted or never run, a failed test still
#     in the log (bit 7), or a drive smartctl cannot read: refused, the drive
#     named by serial. --check shows the same; --force skips the check.
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
BTRDASD_BIN="${BTRDASD_BIN:-/usr/bin/btrdasd}"
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

# The gate in front of --run: every drive's most recent SMART self-test must
# have PASSED (bd DAS-Backup-Manager-25r7). Up to 2.2.1 it blocked only on a
# test still running, so one that failed, was aborted or interrupted, or
# never ran read as done, and --run went on to YES-DESTROY. A drive is named
# by serial, never by /dev/sdX: letters move on every reconnect.
check_smart_tests() {
    log_header "Checking SMART Self-Test Results"

    local all_passed=true serial dev label out rc verdict SELFTEST_STATUS
    for serial in "${!DISCOVERED_DEVICES[@]}"; do
        dev="${DISCOVERED_DEVICES[$serial]}"
        label="${TARGET_LABELS[$serial]}"

        # The output and the exit status, apart (bd DAS-Backup-Manager-jgzx):
        # the status is a bitmask, nonzero for findings that leave the log
        # readable, so `|| echo "No tests"` added a second status to a real
        # one. A capture bash cannot make (no fd to spare) prints nothing and
        # still returns 0 (measured, bash 5.3): no output must block by
        # itself, whatever the status says.
        rc=0
        out="$(smartctl -l selftest "$dev" 2>&1)" || rc=$?

        if selftest_passed "$out" "$rc"; then
            verdict="${GREEN}PASSED${NC}"
        else
            all_passed=false
            # Display only: anything but a pass blocks either way. Matched
            # by bash itself, no pipe and no file, like every match here.
            if [[ $SELFTEST_STATUS == *"in progress"* ]]; then
                verdict="${YELLOW}STILL RUNNING${NC}"
            else
                verdict="${RED}NOT PASSED${NC}"
            fi
        fi
        echo -e "  $serial ($label): $verdict — $SELFTEST_STATUS"
    done

    if ! $all_passed; then
        log_warn "Not every drive has PASSED its most recent SMART self-test."
        return 1
    fi
    log_info "Every drive PASSED its most recent SMART self-test"
    return 0
}

# selftest_passed <smartctl -l selftest output> <its exit status>
# Returns 0 only when the drive's most recent self-test PASSED, and sets
# SELFTEST_STATUS (local to the caller) to what the operator is shown.
selftest_passed() {
    local out="$1" rc="$2" nl=$'\n' row status pass

    # smartctl's exit status is a bitmask (smartctl(8), EXIT STATUS). Bits
    # 0-1: it could not parse its command line or open the drive, so nothing
    # it printed is a reading. 127 (no smartctl) sets both. A signal (128+N)
    # always sets bit 7, which blocks below.
    if ((rc & 3)); then
        SELFTEST_STATUS="smartctl could not read it (exit $rc)"
        return 1
    fi

    # The most recent self-test is row "# 1": smartctl numbers its log
    # newest first, and only its rows start with "#". Found by one regex
    # over the whole output: no pipe, no file (round 4, N3), and no digit
    # class to widen with the locale (N5).
    local re="(^|$nl)(# 1  [^$nl]*)"
    if [[ $out =~ $re ]]; then
        row="${BASH_REMATCH[2]}"
    elif [[ $out == *"No "[Ss]"elf-tests have been logged"* ]]; then
        SELFTEST_STATUS="no self-test logged"
        return 1
    else
        SELFTEST_STATUS="no self-test result in smartctl's output (exit $rc)"
        return 1
    fi

    # Its status is one fixed-width field, in smartctl 7.5's own layouts:
    #   ATA   "#%2u  %-19s %-29s %1d0%%  %8u         %s"   status, columns 25-53
    #   SCSI  "#%2d  %s" (16 wide), "  %s%s" (25 wide), ... " [sense]"
    #                                                     result, columns 23-47
    # Only a SCSI row ends in "]", its sense field. Read whole, a failure
    # cannot pass on a pass's first word: ATA "Completed: read failure",
    # SCSI "Completed, segment failed".
    if [[ $row == *"]" ]]; then
        status="${row:23:25}" pass="Completed"
    else
        status="${row:25:29}" pass="Completed without error"
    fi
    status="${status%"${status##*[! ]}"}"
    SELFTEST_STATUS="${status:-$row}"
    if [[ $status != "$pass" ]]; then
        return 1
    fi

    # A pass, unless the exit status flags more than bit 2 (some other SMART
    # command failed) or bit 6 (the error log has entries), which say
    # nothing of this log. Bit 7: the log holds a failed self-test — on ATA
    # one no later passed extended test outdates, on SCSI any of the last 20.
    # Bits 3-5 (the drive reports failing) come only with -H, never with
    # this call (smartctl 7.5's source); were one set, it blocks all the same.
    if ((rc & 128)); then
        SELFTEST_STATUS="$status, but the log records a failed self-test (smartctl exit $rc)"
        return 1
    fi
    if ((rc & ~(4 | 64))); then
        SELFTEST_STATUS="$status, but smartctl reports a problem (exit $rc)"
        return 1
    fi
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

        local FS_LABEL FS_LABEL_ORIGIN
        read_fs_label "$dev" "$role" "${BTRFS_LABEL_PREFIX}-${label}" || exit 1
        local origin_note="(preserved from disk)"
        [[ "$FS_LABEL_ORIGIN" == "new" ]] && origin_note="(NEW — no existing filesystem found)"

        echo "$name ($dev, serial: $serial):"
        if [[ "$role" == "primary" ]]; then
            echo "  Whole disk BTRFS (single partition) - label: ${FS_LABEL} ${origin_note}"
        elif is_bootable_role "$role"; then
            # Show the ESP label the run would actually write. It is unique per
            # drive and is what identifies this recovery system afterwards, so
            # it belongs in the preview rather than being a surprise at format
            # time.
            local esp_label
            esp_label="$(derive_esp_label "$serial")" || exit 1
            echo "  Partition 1: 1.5G ESP (FAT32) - EFI System Partition, label: $esp_label"
            echo "  Partition 2: remainder BTRFS - label: ${FS_LABEL} ${origin_note}"
        else
            echo "  Whole disk BTRFS - label: ${FS_LABEL} ${origin_note}"
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

# -- ESP label derivation (must be UNIQUE per drive) ------------------
# Each bootable recovery drive carries its own independent OS, so its ESP
# must be addressable on its own. A single shared label makes
# `blkid -t LABEL=<x> -o device` return BOTH partitions, and every consumer
# of that lookup then has to guess which one it meant. The live drives use
# the bay-numbered convention RECOV-ESP-<bay>; this reproduces it.
#
# FAT32 volume labels are capped at 11 bytes, so "RECOV-ESP-<bay>" only fits
# a single-digit bay. Both the length and the uniqueness are checked, and
# both fail closed -- a wrong label here is written to a recovery drive.
ESP_LABEL_PREFIX="RECOV-ESP"
FAT_LABEL_MAX=11

# Single definition of "this target gets an ESP" -- used by both the
# uniqueness pre-check and the partitioning loop, so they cannot disagree.
is_bootable_role() {
    local role="$1"
    [[ "$role" == *"esp"* || "$role" == *"boot"* || "$role" == *"system"* || "$role" == *"mirror"* ]]
}

derive_esp_label() {
    local serial="$1"
    local display="${TARGET_NAMES[$serial]:-}"
    local bay=""

    # Bay number lives in the target's display_name, e.g.
    #   "2TB Recovery A (Bay 1, ZK208Q77)"
    if [[ "$display" =~ [Bb]ay[[:space:]]+([0-9]+) ]]; then
        bay="${BASH_REMATCH[1]}"
    fi

    if [[ -z "$bay" ]]; then
        # stderr, not stdout: this function's stdout IS the label.
        # return, not exit: every caller reaches this through $( ), where an
        # exit would kill only the subshell and hand back an empty label.
        log_error "Cannot derive ESP label for serial $serial: no bay number in display_name ('$display')." >&2
        log_error "Add a display_name containing 'Bay <N>' for this target in config.toml." >&2
        return 1
    fi

    local esp_label="${ESP_LABEL_PREFIX}-${bay}"
    if (( ${#esp_label} > FAT_LABEL_MAX )); then
        log_error "Derived ESP label '$esp_label' is ${#esp_label} chars; FAT32 allows $FAT_LABEL_MAX." >&2
        return 1
    fi

    printf '%s' "$esp_label"
}

# Refuse to touch anything if two bootable targets would collide on one ESP
# label. Checked BEFORE the first destructive step, never per-drive mid-run.
verify_esp_labels_unique() {
    local -A seen=()
    local serial role esp_label
    for serial in "${!DISCOVERED_DEVICES[@]}"; do
        role="${TARGET_ROLES[$serial]}"
        is_bootable_role "$role" || continue
        esp_label="$(derive_esp_label "$serial")" || exit 1
        [[ -n "$esp_label" ]] || {
            log_error "Empty ESP label derived for serial $serial — refusing to continue."
            exit 1
        }
        if [[ -n "${seen[$esp_label]:-}" ]]; then
            log_error "ESP label collision: '$esp_label' derived for BOTH serial ${seen[$esp_label]} and serial $serial."
            log_error "Each bootable drive needs a distinct bay number in its display_name."
            exit 1
        fi
        seen[$esp_label]="$serial"
    done
}

# -- BTRFS filesystem label resolution (must ROUND-TRIP) --------------
# The BTRFS label is the filesystem's identity for everything downstream.
# Building it as "das-backup-" + the config TARGET ID does not reproduce
# what is deployed -- it never matched any drive:
#
#     config target id        would derive                    on disk
#     primary-22tb            das-backup-primary-22tb         das-backup-22tb
#     system-recovery-A-2tb   das-backup-system-recovery-A-2tb  das-backup-system-recovery-A
#     system-recovery-B-2tb   das-backup-system-recovery-B-2tb  das-backup-system-recovery-B
#
# so re-partitioning any target silently RENAMED its filesystem (bd 5j7).
#
# Re-partitioning destroys the data but must reproduce the same IDENTITY,
# or config.toml, btrbk.conf and every operator lookup stop resolving. So
# the live label wins when there is one; the derived name is a fallback
# for a genuinely blank drive only.
#
# Deliberately NOT symmetric with the ESP label above, and the asymmetry is
# the point: the deployed BTRFS labels are known-good and worth preserving,
# whereas the deployed ESP labels were historically a single shared
# "BACKUP-ESP" across both recovery drives -- a value we specifically do
# not want to perpetuate. Preserve a good identity; derive a corrected one.
btrfs_partition_for_role() {
    local role="$1"
    if is_bootable_role "$role"; then echo 2; else echo 1; fi
}

# Prints "<origin>|<label>" so the caller learns both facts through the
# command substitution. A global would not survive the subshell.
resolve_fs_label() {
    local dev="$1" role="$2" fallback="$3"
    local part existing
    part="$(btrfs_partition_for_role "$role")"
    # blkid's empty output conflates two very different answers: "this
    # filesystem has no LABEL" and "blkid could not tell me". Both used to
    # arrive here as an empty string, and the caller then went on to WRITE the
    # fallback label -- so a transient blkid failure on a labelled drive would
    # silently rename it, which is the exact renaming this function exists to
    # prevent (bd DAS-Backup-Manager-5j7). Separate them by exit status:
    # 0 = a label was found, 2 = nothing to report (genuinely unlabelled),
    # anything else (4 = usage/other per blkid(8)) is an error and fails closed.
    local blkid_rc
    existing="$(blkid -s LABEL -o value "${dev}${part}" 2>/dev/null)" || blkid_rc=$?
    blkid_rc=${blkid_rc:-0}
    if (( blkid_rc != 0 && blkid_rc != 2 )); then
        log_error "blkid failed on ${dev}${part} (exit $blkid_rc) — refusing to guess whether a label exists." >&2
        return 1
    fi

    if [[ -n "$existing" ]]; then
        printf 'preserved|%s' "$existing"
        return 0
    fi
    if [[ -z "$fallback" ]]; then
        log_error "No existing BTRFS label on ${dev}${part} and no fallback supplied." >&2
        return 1
    fi
    printf 'new|%s' "$fallback"
}

# Split what resolve_fs_label returns, failing closed on either half.
# Callers use this rather than repeating the parsing three times.
read_fs_label() {   # sets FS_LABEL and FS_LABEL_ORIGIN in the CALLER's scope
    local dev="$1" role="$2" fallback="$3" r
    r="$(resolve_fs_label "$dev" "$role" "$fallback")" || return 1
    FS_LABEL_ORIGIN="${r%%|*}"
    FS_LABEL="${r#*|}"
    [[ -n "$FS_LABEL" && "$FS_LABEL_ORIGIN" != "$r" ]] || {
        log_error "Malformed label resolution for $dev: '$r'"
        return 1
    }
    return 0
}

partition_bootable_drive() {
    local dev="$1"
    local label="$2"
    local esp_label="$3"

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
    log_info "  Formatting ${dev}1 as FAT32 (ESP, label=$esp_label)..."
    mkfs.fat -F32 -n "$esp_label" "${dev}1"

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

    verify_esp_labels_unique

    for serial in "${!DISCOVERED_DEVICES[@]}"; do
        local dev="${DISCOVERED_DEVICES[$serial]}"
        local label="${TARGET_LABELS[$serial]}"
        local role="${TARGET_ROLES[$serial]}"

        local FS_LABEL FS_LABEL_ORIGIN
        read_fs_label "$dev" "$role" "${BTRFS_LABEL_PREFIX}-${label}" || exit 1
        log_info "  BTRFS label: $FS_LABEL ($FS_LABEL_ORIGIN)"

        if [[ "$role" == "primary" ]]; then
            partition_data_drive "$dev" "$FS_LABEL"
        elif is_bootable_role "$role"; then
            local esp_label
            esp_label="$(derive_esp_label "$serial")" || exit 1
            [[ -n "$esp_label" ]] || {
                log_error "Empty ESP label derived for serial $serial — refusing to format."
                exit 1
            }
            partition_bootable_drive "$dev" "$FS_LABEL" "$esp_label"
        else
            partition_data_drive "$dev" "$FS_LABEL"
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

    # Run the ESP-label pre-flight in EVERY mode, --check included, so a
    # misconfigured display_name surfaces in the preview rather than at the
    # moment drives are being formatted. run_partitioning calls it again as a
    # last line of defence for any future caller that bypasses main.
    verify_esp_labels_unique

    case "$mode" in
        --check|-c)
            check_smart_tests || log_warn "--run will refuse until every drive has."
            show_plan
            echo ""
            log_info "Run with --run to execute partitioning"
            ;;
        --run|-r)
            if ! check_smart_tests; then
                log_error "Partitioning blocked: every drive must have PASSED its most recent SMART self-test (see above)."
                log_error "Let a running test finish; run 'smartctl -t long' where none passed; replace a drive whose test failed. --force skips this check."
                exit 1
            fi
            show_plan
            confirm_destruction
            run_partitioning
            ;;
        --force)
            log_warn "Forcing partitioning: SMART self-test results are NOT checked"
            show_plan
            confirm_destruction
            run_partitioning
            ;;
        *)
            echo "Usage: $0 [--check|--run|--force]"
            echo "  --check  Verify drives and show plan (default)"
            echo "  --run    Execute partitioning (every drive's most recent SMART self-test must have PASSED)"
            echo "  --force  Execute partitioning (skip the SMART self-test check)"
            exit 1
            ;;
    esac
}

main "$@"
