#!/bin/bash
# shellcheck disable=SC1090,SC2034,SC2329
# SC1090: $LIB is a generated extract; its path is dynamic by design.
# SC2034: EXPECTED_SERIALS and CANDIDATE_DISKS are consumed by the extracted functions.
# SC2329: list_candidate_disks and dump are invoked by the extracted code and the EXIT trap.
# Drive discovery in das-wipe-all-drives-and-partition-from-scratch.sh (bd
# DAS-Backup-Manager-tjph): which device nodes the script would wipe, chosen
# only by a drive's exact serial from `smartctl -i`, whatever the transport
# prints it as (ATA and NVMe "Serial Number:", SCSI/SAS "Serial number:").
#
# Extracts the real functions (the logic is NOT reimplemented here) and drives
# them with fixture `smartctl -i` outputs. Nothing touches a device: the device
# list is a fixed set of names that need not exist, and the smartctl on PATH is
# a stub that only prints a fixture -- it never runs any other program and never
# calls its own name. Serials are placeholders (SERIALnn).
#
# Run by hand: systemd-run --user --scope --quiet -p TasksMax=600 bash tests/test_partition_discovery.sh
set -uo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/scripts/das-wipe-all-drives-and-partition-from-scratch.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK:?}"' EXIT
LIB="$WORK/lib.sh"
STUB="$WORK/bin"
FIX="$WORK/fixtures"
mkdir -p "$STUB" "$FIX"

# Everything from the log helpers up to the SMART gate: check_root, the
# discovery state, list_candidate_disks, the serial parser, discover_devices,
# verify_serials. Plus the colour variables the log helpers print.
sed -n '/^log_info()/,/^check_smart_tests() {/p' "$SRC" | sed '$d' > "$LIB"
sed -n '/^RED=/p;/^GREEN=/p;/^YELLOW=/p;/^BLUE=/p;/^NC=/p' "$SRC" >> "$LIB"

# A sed range that silently caught nothing would make every refusal case
# "pass" on `command not found`; the extraction is verified first.
for fn in log_info log_warn log_error log_header list_candidate_disks \
    discover_devices verify_serials; do
    grep -q "^$fn()" "$LIB" || {
        echo "HARNESS BROKEN: $fn not extracted"
        exit 2
    }
done
grep -q '^RED=' "$LIB" || {
    echo "HARNESS BROKEN: colour vars not extracted"
    exit 2
}

# The stub: prints $FIXTURE_DIR/<device basename>.out and exits with .rc
# (default 0). Builtins only. It refuses any call that is not `-i <dev>`, so a
# discovery that asked smartctl for something else fails loudly here.
cat > "$STUB/smartctl" <<'EOF'
#!/bin/bash
if [[ $# -ne 2 || $1 != "-i" ]]; then
    printf 'smartctl stub: unexpected arguments: %s\n' "$*" >&2
    exit 2
fi
f="$FIXTURE_DIR/${2##*/}"
if [[ ! -f "$f.out" ]]; then
    printf 'smartctl stub: no fixture for %s\n' "$2" >&2
    exit 2
fi
while IFS= read -r line || [[ -n $line ]]; do
    printf '%s\n' "$line"
done < "$f.out"
rc=0
[[ -f "$f.rc" ]] && read -r rc < "$f.rc"
exit "$rc"
EOF
chmod +x "$STUB/smartctl"

# smartctl 7.5's own layouts (ataprint.cpp, scsiprint.cpp, nvmeprint.cpp).
# Not captured from hardware: the SAS layout is unverified on a real SAS drive.
ata() {
    printf '%s\n' "smartctl 7.5 2025-04-30 r5714 [x86_64-linux] (local build)" \
        "=== START OF INFORMATION SECTION ===" \
        "Model Family:     Seagate BarraCuda 3.5" \
        "Device Model:     ST2000DM008-2FR102" \
        "Serial Number:    $1" \
        "LU WWN Device Id: 5 000c50 0abcdef01" \
        "SMART support is: Enabled"
}
sas() {
    printf '%s\n' "smartctl 7.5 2025-04-30 r5714 [x86_64-linux] (local build)" \
        "=== START OF INFORMATION SECTION ===" \
        "Vendor:               SEAGATE" \
        "Product:              ST4000NM0023" \
        "Revision:             0004" \
        "User Capacity:        4,000,787,030,016 bytes [4.00 TB]" \
        "Logical Unit id:      0x5000c500abcdef01" \
        "Serial number:        $1" \
        "Device type:          disk" \
        "Transport protocol:   SAS (SPL-4)"
}
nvme() {
    printf '%s\n' "smartctl 7.5 2025-04-30 r5714 [x86_64-linux] (local build)" \
        "=== START OF INFORMATION SECTION ===" \
        "Model Number:                       Example NVMe SSD 1TB" \
        "Serial Number:                      $1" \
        "Firmware Version:                   1B2QGXA7"
}
noserial() {
    printf '%s\n' "smartctl 7.5 2025-04-30 r5714 [x86_64-linux] (local build)" \
        "=== START OF INFORMATION SECTION ===" \
        "Vendor:               Generic" \
        "Product:              STORAGE DEVICE"
}

pass=0
fail=0
ok() {
    echo "  PASS: $1"
    pass=$((pass + 1))
}
bad() {
    echo "  FAIL: $1"
    fail=$((fail + 1))
}

# fixture <case> <dev basename> <rc> : the fixture text on stdin
fixture() {
    mkdir -p "$FIX/$1"
    cat > "$FIX/$1/$2.out"
    printf '%s\n' "$3" > "$FIX/$1/$2.rc"
}

# discover <case> <allowed serials, space-separated> <devices, space-separated> [then]
# Runs discover_devices (and, with then=verify, verify_serials after it) in a
# subshell. Prints its output, then "MAP serial=dev" lines sorted, then "RC n".
discover() {
    local case="$1" allowed="$2" devs="$3" then="${4:-}"
    (
        export FIXTURE_DIR="$FIX/$case"
        PATH="$STUB:$PATH"
        source "$LIB"
        # The script's own shell options: an error-exit inside discovery
        # must show up here as it would in a real run.
        set -euo pipefail
        declare -A EXPECTED_SERIALS=()
        local s d
        for s in $allowed; do EXPECTED_SERIALS[$s]="label-$s"; done
        list_candidate_disks() {
            CANDIDATE_DISKS=()
            for d in $devs; do CANDIDATE_DISKS+=("/dev/$d"); done
        }
        # Both functions refuse by `exit 1`: the map and the status are
        # printed on the way out, whichever way the subshell ends.
        dump() {
            local rc=$? k
            for k in "${!DISCOVERED_DEVICES[@]}"; do
                printf 'MAP %s=%s\n' "$k" "${DISCOVERED_DEVICES[$k]}"
            done | sort
            printf 'RC %s\n' "$rc"
        }
        trap dump EXIT
        discover_devices
        if [[ $then == verify ]]; then
            verify_serials
        fi
        exit 0
    ) 2>&1
}

# expect <desc> <output> <want rc> <want MAP lines, ';'-joined, or ''> [required substring]
expect() {
    local desc="$1" out="$2" want_rc="$3" want_map="$4" msg="${5:-}" got_map got_rc
    got_rc="$(sed -n 's/^RC //p' <<< "$out")"
    got_map="$(sed -n 's/^MAP //p' <<< "$out" | paste -sd ';' -)"
    if [[ $got_rc != "$want_rc" ]]; then
        bad "$desc -- exit $got_rc, expected $want_rc; output: $out"
    elif [[ $got_map != "$want_map" ]]; then
        bad "$desc -- discovered '${got_map}', expected '${want_map}'; output: $out"
    elif [[ -n $msg && $out != *"$msg"* ]]; then
        bad "$desc -- message lacked '$msg'; output: $out"
    else
        ok "$desc"
    fi
}

echo "== Found: each transport's serial line, exact match =="
ata SERIAL01 | fixture ata sdb 0
expect "ATA 'Serial Number:' on the list is found" \
    "$(discover ata "SERIAL01" "sdb")" 0 "SERIAL01=/dev/sdb"

sas SERIAL03 | fixture sas sdc 0
expect "SAS/SCSI 'Serial number:' on the list is found" \
    "$(discover sas "SERIAL03" "sdc")" 0 "SERIAL03=/dev/sdc"

nvme SERIAL05 | fixture nvme sdd 0
expect "NVMe-layout 'Serial Number:' on the list is found" \
    "$(discover nvme "SERIAL05" "sdd")" 0 "SERIAL05=/dev/sdd"

{
    ata SERIAL01
} | fixture mixed sdb 0
sas SERIAL03 | fixture mixed sdc 0
ata SERIAL09 | fixture mixed sde 0
expect "ATA and SAS drives together; an unlisted drive beside them is left alone" \
    "$(discover mixed "SERIAL01 SERIAL03" "sdb sdc sde")" 0 "SERIAL01=/dev/sdb;SERIAL03=/dev/sdc"

sas SERIAL03 | fixture verify sdc 0
expect "a SAS drive passes verify_serials end to end" \
    "$(discover verify "SERIAL03" "sdc" verify)" 0 "SERIAL03=/dev/sdc" "All drive serials verified"

echo "== Identity rule: padding is not identity; case and every other character are =="
printf 'Serial Number:    SERIAL01   \n' | fixture trail sdb 0
expect "trailing spaces after the serial are padding: found" \
    "$(discover trail "SERIAL01" "sdb")" 0 "SERIAL01=/dev/sdb"

ata serial01 | fixture lower sdb 0
expect "a serial differing only in case is a different serial: not found" \
    "$(discover lower "SERIAL01" "sdb")" 0 ""

ata "SERIAL01 EXTRA" | fixture prefix sdb 0
expect "a listed serial that is only the first word of the value is not a match" \
    "$(discover prefix "SERIAL01" "sdb")" 0 ""

printf 'Bridge Serial Number: SERIAL01\n' | fixture anchor sdb 0
expect "a serial line must start the line ('Bridge Serial Number:' is not the drive's)" \
    "$(discover anchor "SERIAL01" "sdb")" 0 "" "no serial"

echo "== Refused: not on the list, unreadable, ambiguous, duplicated =="
ata SERIAL09 | fixture unlisted sdb 0
expect "a serial not on the list is not discovered" \
    "$(discover unlisted "SERIAL01" "sdb")" 0 ""
expect "... and verify_serials then refuses the run (NOT FOUND)" \
    "$(discover unlisted "SERIAL01" "sdb" verify)" 1 "" "NOT FOUND"

noserial | fixture none sdb 0
expect "output with no serial line: not discovered, named in a warning" \
    "$(discover none "SERIAL01" "sdb")" 0 "" "/dev/sdb: no serial"
expect "... and verify_serials then refuses the run" \
    "$(discover none "SERIAL01" "sdb" verify)" 1 "" "NOT FOUND"

printf 'Serial Number:   \n' | fixture empty sdb 0
expect "an empty serial value is no serial: not discovered" \
    "$(discover empty "SERIAL01" "sdb")" 0 "" "/dev/sdb: no serial"

{
    ata SERIAL01
    printf 'Serial number:        SERIAL07\n'
} | fixture ambig sdb 0
expect "two different serial lines, one of them listed: the whole discovery refuses" \
    "$(discover ambig "SERIAL01" "sdb")" 1 "" "more than one serial"

{
    ata SERIAL08
    printf 'Serial number:        SERIAL07\n'
} | fixture ambig2 sdb 0
expect "two different serial lines, neither listed: not discovered, named in a warning" \
    "$(discover ambig2 "SERIAL01" "sdb")" 0 "" "more than one serial"

{
    ata SERIAL01
    ata SERIAL01
} | fixture twice sdb 0
expect "the same serial line twice is still more than one: refuses" \
    "$(discover twice "SERIAL01" "sdb")" 1 "" "more than one serial"

ata SERIAL01 | fixture dup sdb 0
ata SERIAL01 | fixture dup sdc 0
expect "one listed serial on two devices (dual-path SAS, a bridge's serial): refuses" \
    "$(discover dup "SERIAL01" "sdb sdc")" 1 "" "on more than one device"

ata SERIAL01 | fixture norc sdb 2
expect "smartctl could not open the drive (exit 2), a listed serial printed anyway: refuses" \
    "$(discover norc "SERIAL01" "sdb")" 1 "" "could not read"

noserial | fixture norc2 sdb 2
expect "smartctl could not open the drive (exit 2), no listed serial: not discovered" \
    "$(discover norc2 "SERIAL01" "sdb")" 0 "" "could not read"

for d in sdb sdc sdd; do ata SERIAL01 | fixture dup3 "$d" 0; done
ata SERIAL03 | fixture dup3 sde 0
expect "one listed serial on three devices: none of them is kept, the run refused" \
    "$(discover dup3 "SERIAL01 SERIAL03" "sdb sdc sdd sde" verify)" 1 "SERIAL03=/dev/sde" "on more than one device"

ata SERIAL01 | fixture rc4 sdb 4
expect "a SMART finding bit (exit 4) does not make the identity unreadable: found" \
    "$(discover rc4 "SERIAL01" "sdb")" 0 "SERIAL01=/dev/sdb"

expect "a device the stub has no fixture for (smartctl exit 2, no output): not discovered" \
    "$(discover missing "SERIAL01" "sdz")" 0 "" "could not read"

echo ""
echo "RESULT: $pass passed, $fail failed"
[[ $fail -eq 0 ]]
