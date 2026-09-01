#!/bin/bash
# shellcheck disable=SC2016,SC1090,SC2034
# SC2016: fixtures are single-quoted ON PURPOSE — they are eval'd inside the
#   test subshell, so expanding them here would defeat the whole harness.
# SC1090: $LIB is a generated extract; its path is dynamic by design.
# SC2034: the TARGET_* arrays are consumed by the extracted functions.
# Falsification harness for derive_esp_label / verify_esp_labels_unique.
#
# Extracts the real functions from das-partition-drives.sh (the logic is NOT
# reimplemented here) and drives them with controlled fixtures.
#
# Two anti-false-pass measures, both learned the hard way while writing this:
#   1. The extraction is verified — a sed range that silently caught nothing
#      made every "expected failure" case pass on `command not found`.
#   2. A failing case must exit nonzero AND print that specific guard's
#      message. Bare `rc != 0` also matches an unbound variable or a syntax
#      error, so on its own it proves nothing about which path ran.
set -uo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/scripts/das-partition-drives.sh"
LIB="$(mktemp)"
trap 'rm -f "$LIB"' EXIT

sed -n '/^ESP_LABEL_PREFIX=/,/^partition_bootable_drive() {/p' "$SRC" | sed '$d' > "$LIB"
sed -n '/^log_error()/p;/^RED=/p;/^GREEN=/p;/^YELLOW=/p;/^BLUE=/p;/^NC=/p' "$SRC" >> "$LIB"

for fn in is_bootable_role derive_esp_label verify_esp_labels_unique log_error \
           btrfs_partition_for_role resolve_fs_label read_fs_label; do
    grep -q "^$fn()" "$LIB" || { echo "HARNESS BROKEN: $fn not extracted"; exit 2; }
done
grep -q '^RED=' "$LIB" || { echo "HARNESS BROKEN: colour vars not extracted"; exit 2; }

pass=0; fail=0
ok()  { echo "  PASS: $1"; pass=$((pass+1)); }
bad() { echo "  FAIL: $1"; fail=$((fail+1)); }

# accept <desc> <fixture> <exact-expected-stdout>
accept() {
    local desc="$1" fixture="$2" want="$3" out rc
    out=$( source "$LIB"
           declare -A TARGET_NAMES=() TARGET_ROLES=() DISCOVERED_DEVICES=()
           eval "$fixture" ) ; rc=$?
    if [[ $rc -ne 0 ]]; then
        bad "$desc — expected success, exited $rc ('$out')"
    elif [[ "$out" != "$want" ]]; then
        bad "$desc — expected '$want', got '$out'"
    else
        ok "$desc -> '${out:-<no output, as expected>}'"
    fi
}

# refuse <desc> <fixture> <required-substring-of-that-guard's-message>
refuse() {
    local desc="$1" fixture="$2" msg="$3" out rc
    # 2>&1 is required: derive_esp_label writes its diagnostics to stderr on
    # purpose, because its stdout is the label it returns. A refusal has to be
    # asserted wherever the code actually sends it.
    out=$( source "$LIB"
           declare -A TARGET_NAMES=() TARGET_ROLES=() DISCOVERED_DEVICES=()
           eval "$fixture" 2>&1 ) ; rc=$?
    if [[ $rc -eq 0 ]]; then
        bad "$desc — guard did NOT fire (exit 0)"
    elif [[ "$out" != *"$msg"* ]]; then
        bad "$desc — exited $rc but message lacked '$msg'; got: ${out:-<empty>}"
    else
        ok "$desc (exit $rc, correct guard)"
    fi
}

echo "== SUCCESS direction: real config display_name strings, vs what is on disk =="
accept "bay 1 -> RECOV-ESP-1 (matches /dev/sdj1 today)" \
    'TARGET_NAMES[ZK208Q77]="2TB Recovery A (Bay 1, ZK208Q77)"; derive_esp_label ZK208Q77' \
    "RECOV-ESP-1"
accept "bay 4 -> RECOV-ESP-4 (matches /dev/sdi1 today)" \
    'TARGET_NAMES[ZFL41DNY]="2TB Recovery B (Bay 4, ZFL41DNY)"; derive_esp_label ZFL41DNY' \
    "RECOV-ESP-4"
accept "lowercase 'bay' also matches" \
    'TARGET_NAMES[X]="recovery (bay 3)"; derive_esp_label X' \
    "RECOV-ESP-3"

echo "== FAILURE direction: each guard must fire, and be the one that fired =="
refuse "no bay in display_name" \
    'TARGET_NAMES[X]="2TB Recovery A (ZK208Q77)"; derive_esp_label X' \
    "no bay number in display_name"
refuse "empty display_name" \
    'TARGET_NAMES[X]=""; derive_esp_label X' \
    "no bay number in display_name"
refuse "missing display_name entry entirely" \
    'derive_esp_label NOSUCHSERIAL' \
    "no bay number in display_name"
refuse "two-digit bay exceeds the FAT 11-byte cap" \
    'TARGET_NAMES[X]="Recovery (Bay 12)"; derive_esp_label X' \
    "FAT32 allows"
refuse "two bootable targets deriving the same label" \
    'TARGET_NAMES[A]="Recovery A (Bay 1)"; TARGET_ROLES[A]="mirror"; DISCOVERED_DEVICES[A]=/dev/null
     TARGET_NAMES[B]="Recovery B (Bay 1)"; TARGET_ROLES[B]="mirror"; DISCOVERED_DEVICES[B]=/dev/null
     verify_esp_labels_unique' \
    "ESP label collision"

echo "== SUBSHELL PROPAGATION: a refusal must escape the command substitution =="
# derive_esp_label is always reached through command substitution, and an
# "exit 1" inside one only terminates the subshell. If the refusal does not
# propagate to the caller, an EMPTY label reaches the formatting step.
refuse "underivable target aborts verify_esp_labels_unique, not just its subshell" \
    'TARGET_NAMES[BAD]="Recovery with no bay number"; TARGET_ROLES[BAD]="mirror"; DISCOVERED_DEVICES[BAD]=/dev/null
     verify_esp_labels_unique
     echo "SURVIVED-THE-GUARD"' \
    "no bay number in display_name"
refuse "underivable target never yields an empty label to its caller" \
    'TARGET_NAMES[BAD]="Recovery with no bay number"
     lbl=$(derive_esp_label BAD) || exit 1
     [[ -z "$lbl" ]] && { echo "EMPTY-LABEL-LEAKED"; exit 9; }
     echo "SURVIVED-THE-GUARD"' \
    "no bay number in display_name"

echo "== BTRFS LABEL ROUND-TRIP (bd 5j7): the live label must win =="
# blkid is stubbed so the partition it is asked about is observable. The bug
# being guarded against is a re-partition silently RENAMING a filesystem.
accept "existing label on a data target is preserved verbatim" \
    'blkid() { echo "das-backup-22tb"; }
     resolve_fs_label /dev/sdl primary "das-backup-primary-22tb"' \
    "preserved|das-backup-22tb"
accept "existing label on a bootable target is preserved verbatim" \
    'blkid() { echo "das-backup-system-recovery-A"; }
     resolve_fs_label /dev/sdj mirror "das-backup-system-recovery-A-2tb"' \
    "preserved|das-backup-system-recovery-A"
accept "blank drive falls back to the derived name, flagged as new" \
    'blkid() { return 2; }
     resolve_fs_label /dev/sdz primary "das-backup-fresh"' \
    "new|das-backup-fresh"
accept "data role reads partition 1" \
    'blkid() { echo "PART:${!#}"; }
     resolve_fs_label /dev/sdl primary fb' \
    "preserved|PART:/dev/sdl1"
accept "bootable role reads partition 2" \
    'blkid() { echo "PART:${!#}"; }
     resolve_fs_label /dev/sdj mirror fb' \
    "preserved|PART:/dev/sdj2"

refuse "blank drive with no fallback is refused, not silently unlabelled" \
    'blkid() { return 2; }
     resolve_fs_label /dev/sdz primary ""' \
    "no fallback supplied"
refuse "read_fs_label propagates that refusal through the substitution" \
    'blkid() { return 2; }
     read_fs_label /dev/sdz primary "" || exit 1
     echo "SURVIVED-THE-GUARD"' \
    "no fallback supplied"

echo "== STRUCTURAL: every read_fs_label call site must check the status =="
# read_fs_label returns rather than exits, by design -- so an unchecked call
# site would silently proceed with a stale/empty FS_LABEL. Assert on the
# source, because no fixture can catch a call site nobody wrote a test for.
bad_sites=$(grep -n 'read_fs_label ' "$SRC" | grep -v '^\s*#' | grep -v '||' || true)
if [[ -n "$bad_sites" ]]; then
    bad "read_fs_label call site does not check its status: $bad_sites"
else
    ok "all read_fs_label call sites check their status"
fi

echo "== CONTROL: the guards must NOT fire on valid input =="
accept "two bootable targets, distinct bays" \
    'TARGET_NAMES[A]="Recovery A (Bay 1)"; TARGET_ROLES[A]="mirror"; DISCOVERED_DEVICES[A]=/dev/null
     TARGET_NAMES[B]="Recovery B (Bay 4)"; TARGET_ROLES[B]="mirror"; DISCOVERED_DEVICES[B]=/dev/null
     verify_esp_labels_unique' \
    ""
accept "non-bootable primary skipped even with no bay in its name" \
    'TARGET_NAMES[P]="22TB Exos RAID-1"; TARGET_ROLES[P]="primary"; DISCOVERED_DEVICES[P]=/dev/null
     verify_esp_labels_unique' \
    ""

echo
echo "passed=$pass failed=$fail"
[[ $fail -eq 0 ]] || exit 1
echo "ESP LABEL SUITE GREEN"
