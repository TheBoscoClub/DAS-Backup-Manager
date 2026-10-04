#!/bin/bash
# shellcheck disable=SC2016,SC2030,SC2031,SC2034,SC2329
# SC2016: the lines extract() looks for are literal text, single-quoted so.
# SC2030, SC2031: every LC_ALL set here is meant to stay in its subshell.
# SC2034, SC2329: the globals and stub functions each case defines are read
#   and called by the function it sourced, which shellcheck cannot see.
# A reader that quits early after a pipe, under `set -o pipefail`
# (bd DAS-Backup-Manager-wkvz).
#
#   if echo "$big" | grep -q PATTERN; then ...
#
# grep -q exits at its first match. A producer still writing then dies of
# SIGPIPE (141), pipefail makes the pipeline 141, and the `if` reads that as
# "no match". It takes input larger than a pipe (64 KiB) with a match before
# its end — several lines, since grep reads to a line's end before it matches
# (a single 70 KB line matched 100 of 100) — and then it happens every time
# (measured: 141 in 100 of 100).
#
# Each of the four sites runs here as its script runs it: the real function,
# extracted from the script, under the script's own `set -euo pipefail`, on
# input over 64 KiB that matches on its FIRST line. Each must take the
# matching branch. Every branch also runs on small input, so the fix is shown
# to change nothing else.
#
#   backup-run.sh          update_boot_subvolumes  the drift check on a
#                          target's subvolume listing (exposed: the primary
#                          target's listing is near 64 KiB and grows)
#   backup-verify.sh       check_smart_health      the SMART health line
#   das-partition-drives.sh check_smart_tests      the self-test gate
#
# The health line is latent: smartctl -H prints one, so its own input never
# reaches 64 KiB in several lines, and the large case hands the condition the
# input it would need. The self-test gate pipes nothing any more: it takes
# smartctl's whole output and finds row "# 1" itself (bd 25r7, jgzx), so its
# large cases are real input. Its section also holds the gate's own tests:
# every status smartctl prints, ATA and SCSI, and smartctl's exit status.
#
# The sites are matched by bash itself (`[[ ]]`), not by a here-string: a
# here-string needs a temp file (over 64 KiB) or a pipe, and when bash
# cannot make one — /tmp full, no fd to spare — the `if` reads "no match"
# just the same (round 4, N3). Both are tested below.
#
# Writes only beneath a mktemp directory. No root, no devices.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)" && [[ -d "$WORK" ]] || {
    echo "HARNESS BROKEN: no temp dir"
    exit 2
}
trap 'rm -rf "${WORK:?}"' EXIT

pass=0
fail=0
ok() {
    echo "ok   $1"
    pass=$((pass + 1))
}
bad() {
    echo "FAIL $1"
    fail=$((fail + 1))
}
check() { # check <name> <got> <want>
    if [[ "$2" == "$3" ]]; then
        ok "$1"
    else
        bad "$1 — got '$2', want '$3'"
    fi
}
harness_broken() {
    echo "HARNESS BROKEN: $*"
    exit 2
}

# extract <script> <function> <a line the function must contain>: the
# function's text, into $WORK/<function>.sh. A range that caught nothing, or
# caught a function without the line under test, stops the suite.
extract() {
    local out="$WORK/$2.sh"
    sed -n "/^$2() {/,/^}/p" "$ROOT/scripts/$1" >"$out"
    [[ -s "$out" ]] || harness_broken "$2 not found in $1"
    grep -qF -- "$3" "$out" || harness_broken "$2 in $1 no longer contains: $3"
}
# extract_upto <script> <function> <next function> <a line it must contain>:
# the text from <function> up to <next function>, into $WORK/<function>.sh —
# the function and any helper defined after it. The range must end at
# <next function>, or it ran to the end of the script.
extract_upto() {
    local out="$WORK/$2.sh" text
    text="$(sed -n "/^$2() {/,/^$3() {/p" "$ROOT/scripts/$1")"
    [[ $text == "$2() {"*$'\n'"$3() {" ]] || harness_broken "$2() .. $3() not found in $1"
    printf '%s\n' "${text%"$3() {"}" >"$out"
    grep -qF -- "$4" "$out" || harness_broken "$2 in $1 no longer contains: $4"
}

# A file of <n> filler lines that match nothing below: no 8-digit date, and
# none of the words the conditions look for.
filler() { # filler <lines>
    local i
    for ((i = 0; i < $1; i++)); do
        printf 'ID %06d gen 4242 top level 5 path data/plain-subvolume-%06d\n' "$i" "$i"
    done
}
big_size_ok() { # big_size_ok <file>: over 64 KiB, or the case proves nothing
    local n
    n="$(wc -c <"$1")"
    ((n > 65536)) || harness_broken "$1 is $n bytes, not over 64 KiB"
}

# ---------------------------------------------------------------------------
echo "== backup-run.sh: update_boot_subvolumes, the drift check"
# ---------------------------------------------------------------------------
# A target whose listing HAS btrbk-shaped snapshots that match neither name
# pattern is a drift, a FAIL; one with none is a quiet skip.
extract backup-run.sh update_boot_subvolumes "Target HAS btrbk-shaped snapshots but none matched"
extract backup-run.sh record_op 'OP_STATUS[$op]="$result"'

run_boot_subvols() { # run_boot_subvols <listing file> [tmp-full]: "<result>|<detail>"
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/update_boot_subvolumes.sh"
        # shellcheck source=/dev/null
        source "$WORK/record_op.sh"
        LISTING="$1"
        declare -A OP_STATUS=()
        declare -A MOUNT_ROLES=([/mnt/t]=primary)
        ALL_TARGET_MOUNTS=(/mnt/t)
        mountpoint() { return 0; }
        btrfs() {
            case "$1 $2" in
            "filesystem label") echo das-backup-test ;;
            "subvolume list") cat "$LISTING" ;;
            *)
                echo "btrfs stub: unexpected: $*" >&2
                return 99
                ;;
            esac
        }
        log_info() { echo "[INFO] $*"; }
        log_warn() { echo "[WARN] $*"; }
        log_error() { echo "[ERROR] $*"; }
        # A full /tmp, as bash meets it: no file it writes may pass 4 KiB,
        # and the write fails (SIGXFSZ ignored) instead of killing it.
        if [[ "${2:-}" == tmp-full ]]; then
            trap '' XFSZ
            ulimit -f 4
        fi
        # A locale of the caller's choosing (BOOT_LOCALE), as a unit inherits
        # the host's: under en_US.UTF-8 bash's regex [0-9] matches more.
        if [[ -n "${BOOT_LOCALE:-}" ]]; then
            export LC_ALL="$BOOT_LOCALE"
        fi
        update_boot_subvolumes true >"$WORK/boot.out" 2>&1
        printf '%s|%s\n' "${OP_STATUS[boot_subvols]:-unset}" "${OP_STATUS[boot_subvols_detail]:-}"
    )
}
DRIFTED='ID 300 gen 9 top level 5 path nvme/renamed-root.20261004T0300'

{
    echo "$DRIFTED"
    filler 1500
} >"$WORK/drift-big.txt"
big_size_ok "$WORK/drift-big.txt"
check "boot subvolumes, drift on line 1 of a listing over 64 KiB: FAIL" \
    "$(run_boot_subvols "$WORK/drift-big.txt")" "FAIL|0 updated, 1 failed"
check "boot subvolumes, drift over 64 KiB: says it drifted" \
    "$(grep -c 'HAS btrbk-shaped snapshots but none matched' "$WORK/boot.out")" "1"

# The same listing with /tmp full: a here-string that large went to a temp
# file, could not be written ("here-document: No space left on device"),
# and the check read "no match" — the quiet branch again, 5 of 5 times
# measured (round 4, N3). `[[ =~ ]]` writes nothing.
check "boot subvolumes, drift over 64 KiB with /tmp full: still FAIL" \
    "$(run_boot_subvols "$WORK/drift-big.txt" tmp-full)" "FAIL|0 updated, 1 failed"

echo "$DRIFTED" >"$WORK/drift-small.txt"
check "boot subvolumes, drift in a small listing: FAIL" \
    "$(run_boot_subvols "$WORK/drift-small.txt")" "FAIL|0 updated, 1 failed"

filler 1500 >"$WORK/none-big.txt"
big_size_ok "$WORK/none-big.txt"
check "boot subvolumes, no btrbk snapshots in a listing over 64 KiB: the quiet skip" \
    "$(run_boot_subvols "$WORK/none-big.txt")" "OK|0 updated, 1 skipped"
check "boot subvolumes, no btrbk snapshots: says it skips" \
    "$(grep -c 'No btrbk snapshots found, skipping' "$WORK/boot.out")" "1"

# ---------------------------------------------------------------------------
echo "== backup-verify.sh: check_smart_health, the health line"
# ---------------------------------------------------------------------------
extract backup-verify.sh check_smart_health 'all_passed=false'

run_health() { # run_health <smartctl -H output file>: the verdict line
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/check_smart_health.sh"
        HEALTH_OUT="$1"
        RED="" GREEN="" YELLOW="" BLUE="" NC=""
        DAS_DEVICES=(/dev/sdz)
        declare -A DRIVE_MAP=([TESTSERIAL]="test drive")
        SMART_FAILED=false
        smartctl() {
            case "$1" in
            -H) cat "$HEALTH_OUT" ;;
            -l) echo "# 1  Short offline       Completed without error       00%      1000         -" ;;
            *) return 0 ;;
            esac
        }
        read_drive_serial() { echo TESTSERIAL; }
        smart_attr_raw() { echo 0; }
        report_sector_attr() { return 0; }
        format_attr() { echo "$1${2:-}"; }
        log_header() { :; }
        log_info() { echo "[INFO] $*"; }
        log_warn() { echo "[WARN] $*"; }
        check_smart_health >"$WORK/health.out" 2>&1
        if [[ "$SMART_FAILED" == true ]]; then echo "issues"; else echo "passed"; fi
    )
}
PASSED_LINE='SMART overall-health self-assessment test result: PASSED'

{
    echo "$PASSED_LINE"
    for ((i = 0; i < 1500; i++)); do
        printf 'SMART overall-health self-assessment test result: line %06d\n' "$i"
    done
} >"$WORK/health-big.txt"
big_size_ok "$WORK/health-big.txt"
check "SMART health, PASSED on line 1 of output over 64 KiB: passed" \
    "$(run_health "$WORK/health-big.txt")" "passed"
check "SMART health, over 64 KiB: the PASSED line" \
    "$(grep -c '^  Health: PASSED$' "$WORK/health.out")" "1"

echo "$PASSED_LINE" >"$WORK/health-passed.txt"
check "SMART health, one PASSED line: passed" "$(run_health "$WORK/health-passed.txt")" "passed"
echo 'SMART overall-health self-assessment test result: FAILED!' >"$WORK/health-failed.txt"
check "SMART health, one FAILED line: issues" "$(run_health "$WORK/health-failed.txt")" "issues"
: >"$WORK/health-none.txt"
check "SMART health, no health line: issues" "$(run_health "$WORK/health-none.txt")" "issues"

# ---------------------------------------------------------------------------
echo "== das-partition-drives.sh: check_smart_tests, the self-test gate"
# ---------------------------------------------------------------------------
# The gate in front of --run and its YES-DESTROY prompt (bd
# DAS-Backup-Manager-25r7). A drive passes only when its most recent
# self-test PASSED: ATA "Completed without error", SCSI "Completed".
# Everything else blocks: a test still running, failed, aborted, interrupted
# or never run, a drive smartctl cannot read, and a pass smartctl's exit
# status contradicts. Up to 2.2.1 only "in progress" blocked, so every failure
# went on to YES-DESTROY (round 4 review, gate.sh). Round 3's "self-test
# neither: shown as it is" pinned that: an aborted test passed, rc 0.
#
# smartctl's output and its exit status are read apart (bd jgzx). The status
# is a bitmask (smartctl(8)): bits 0-1, it could not read the drive; bit 7,
# the log holds a failed self-test; bits 2 and 6 say nothing of this log.
#
# Every row is printed with smartctl 7.5's own format strings and status
# words (ataprint.cpp, scsiprint.cpp), and exits as smartctl 7.5 exits for
# it. The ATA layout, and a sample of exit statuses, were checked byte for
# byte against the smartctl 7.5 binary itself, run on a replayed drive (its
# "-" device reads a "-r ataioctl,2" dump from stdin and touches no drive):
# rows, a whole log, the never-run line, the checksum warnings. The SCSI
# rows rest on scsiprint.cpp's format strings alone. Each goes through the
# real check_smart_tests and the helper defined after it, with smartctl a
# stub per drive (taking -b exit as the binary does), no smartctl on PATH,
# and device paths that are not devices.
extract_upto das-partition-drives.sh check_smart_tests show_plan 'smartctl -l selftest'
extract das-partition-drives.sh main 'confirm_destruction'

# The two layouts, as printf strings from smartctl 7.5 itself:
#   ATA   "#%2u  %-19s %-29s %1d0%%  %8u         %s"
#   SCSI  "#%2d  %s" "  %s%s" then " %3u" or "   -", "   %5d" or "     NOW",
#         "%18s" or "                 -", " [0x%x 0x%x 0x%x]" or " [-   -    -]"
ata_row() { # ata_row <num> <test> <status> <remaining, tenths> <LBA or ->
    printf '#%2u  %-19s %-29s %1d0%%  %8u         %s\n' "$1" "$2" "$3" "$4" 1000 "$5"
}
scsi_row() { # scsi_row <num> <code, 16 wide> <result, 25 wide> <segment|-> <hours|NOW> <LBA|-> <sense|->
    local seg='   -' tm lba='                 -' sense=' [-   -    -]'
    [[ $4 == - ]] || printf -v seg ' %3u' "$4"
    if [[ $5 == NOW ]]; then tm='     NOW'; else printf -v tm '   %5d' "$5"; fi
    [[ $6 == - ]] || printf -v lba '%18s' "$6"
    [[ $7 == - ]] || sense=" [$7]"
    printf '#%2d  %s  %s%s%s%s%s\n' "$1" "$2" "$3" "$seg" "$tm" "$lba" "$sense"
}
BANNER='smartctl 7.5 2025-04-30 r5714 [x86_64-linux-7.2.8-1-cachyos] (local build)
Copyright (C) 2002-25, Bruce Allen, Christian Franke, www.smartmontools.org
'
ata_log() { # ata_log <rows>: what `smartctl -l selftest` prints for an ATA drive
    printf '%s\n' "$BANNER" '=== START OF READ SMART DATA SECTION ===' \
        'SMART Self-test log structure revision number 1' \
        'Num  Test_Description    Status                  Remaining  LifeTime(hours)  LBA_of_first_error' "$1"
}
# What smartctl prints by default (-b warn) when a structure it reads fails
# its checksum: its warning where it reads that structure (IDENTIFY and SMART
# data before the section line, the self-test log after it), then the log as
# ever, at exit 0. Under -b exit that warning is its last line, at exit 4: the
# stub below does so.
ata_log_bad_checksum() { # ata_log_bad_checksum <structure> <rows>
    local log section='=== START OF READ SMART DATA SECTION ===' warning="Warning! $1 error: invalid SMART checksum."
    log="$(ata_log "$2")"
    if [[ $1 == "SMART Self-Test Log Structure" ]]; then
        printf '%s\n' "${log/"$section"/"$section"$'\n'"$warning"}"
    else
        printf '%s\n' "${log/"$section"/"$warning"$'\n'"$section"}"
    fi
}
scsi_log() { # scsi_log <rows>: what it prints for a SCSI drive
    printf '%s\n' "$BANNER" '=== START OF READ SMART DATA SECTION ===' 'SMART Self-test log' \
        'Num  Test              Status                 segment  LifeTime  LBA_first_err [SK ASC ASQ]' \
        '     Description                              number   (hours)' "$1"
}
fixture() { printf '%s\n' "$2" >"$WORK/st-$1.txt"; } # fixture <name> <smartctl's output>

# The only programs the gate may find: the three the 2.2.1 gate piped
# through, so that it ran here as it ran on a host. Never smartctl.
mkdir -p "$WORK/bin"
for tool in grep head cat; do
    found="$(type -P "$tool")" || harness_broken "no $tool on PATH"
    ln -s "$found" "$WORK/bin/$tool"
done

# gate_drives <serial>:<fixture>:<smartctl exit status>...: what the gate
# reads, set up in the caller's subshell. PATH is narrowed last, to
# $WORK/bin: no real smartctl can run, whatever becomes of the stub.
gate_drives() {
    RED="" GREEN="" YELLOW="" BLUE="" NC=""
    declare -gA DISCOVERED_DEVICES=() TARGET_LABELS=() SMART_OUT=() SMART_RC=()
    local drive serial rest dev
    for drive in "$@"; do
        serial=${drive%%:*} rest=${drive#*:}
        dev="$WORK/not-a-device-$serial"
        DISCOVERED_DEVICES[$serial]=$dev
        TARGET_LABELS[$serial]="label-$serial"
        SMART_OUT[$dev]="$(<"$WORK/st-${rest%%:*}.txt")"
        SMART_RC[$dev]=${rest#*:}
    done
    : >"$WORK/smartctl.calls"
    smartctl() {
        echo "$*" >>"$WORK/smartctl.calls"
        local out="${SMART_OUT[${!#}]}" rc="${SMART_RC[${!#}]}"
        # -b exit: smartctl stops at its first invalid checksum's warning and
        # exits 4 (FAILSMART: smartctl.cpp checksumwarning(), main()'s catch)
        if [[ " $* " == *" -b exit "* && $out == *"invalid SMART checksum."* ]]; then
            out="${out%%invalid SMART checksum.*}invalid SMART checksum."
            rc=4
        fi
        printf '%s\n' "$out"
        return "$rc"
    }
    log_header() { :; }
    log_info() { echo "[INFO] $*"; }
    log_warn() { echo "[WARN] $*"; }
    log_error() { echo "[ERROR] $*"; }
    PATH="$WORK/bin"
}
gate() { # gate <serial>:<fixture>:<status>...: what the real gate returned; its output in $WORK/gate.out
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/check_smart_tests.sh"
        gate_drives "$@"
        rc=0
        check_smart_tests >"$WORK/gate.out" 2>&1 || rc=$?
        echo "$rc"
    )
}
said() { # said <serial>: what the last gate said of that drive, after its name
    local line
    line="$(grep -F -- "  $1 (label-$1): " "$WORK/gate.out")" || {
        echo "<no line for $1>"
        return 0
    }
    if [[ $line == *$'\n'* ]]; then
        echo "<more than one line for $1>"
    else
        echo "${line#"  $1 (label-$1): "}"
    fi
}
# advised <serial>: what the last gate said to do, on the line under that
# drive ("→ …"), or "-" when it printed none.
advised() {
    local next
    next="$(grep -F -A1 -- "  $1 (label-$1): " "$WORK/gate.out" | sed -n 2p)"
    if [[ $next == "      → "* ]]; then echo "${next#"      → "}"; else echo "-"; fi
}
# What to do, by why a drive did not pass (round 3): each says what its state
# means. A failure older than the newest passed extended test is ignored on
# ATA, so one that passes now supersedes it; SCSI counts every failure among
# its 20 entries, so no newer test does (ataprint.cpp, scsiprint.cpp).
declare -A ADVICE=(
    [none]="-"
    [running]="a test is still running: wait for it to finish, then check again"
    [never-run]="no self-test has run: run an extended test (smartctl -t long) and wait for it to PASS"
    [unfinished]="the test stopped before it finished: run an extended test (smartctl -t long) and wait for it to PASS"
    [failed]="the drive failed its most recent self-test: do not use it, or --force deliberately"
    [unrecognised]="a result this script does not recognise: run an extended test (smartctl -t long) and wait for it to PASS"
    [older-ata]="an older self-test failed and no newer extended test has passed: run an extended test (smartctl -t long), which supersedes that failure only if it PASSES"
    [older-scsi]="an older self-test in its log failed, and on SCSI no newer test supersedes it (smartctl counts it while it is among the last 20): do not use the drive, or --force deliberately"
    [unreadable]="smartctl could not read the drive, so nothing about it is verified: do not use it, or --force deliberately"
    [checksum]="smartctl found an invalid checksum, so its readings cannot be trusted: do not use the drive, or --force deliberately"
    [no-result]="smartctl gave no self-test result, so nothing is verified: do not use the drive, or --force deliberately"
    [flagged]="smartctl reports a problem with the drive: do not use it, or --force deliberately"
)
# decide <fixture> <smartctl exit status> [no-fd]: the decision alone,
# "<returned>|<status shown>"; with no-fd, with no fd to spare (ulimit -n 3).
decide() {
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/check_smart_tests.sh"
        out="$(<"$WORK/st-$1.txt")"
        SELFTEST_STATUS=""
        if [[ ${3:-} == no-fd ]]; then ulimit -n 3; fi
        r=0
        selftest_passed "$out" "$2" || r=$?
        printf '%s|%s\n' "$r" "$SELFTEST_STATUS"
    ) 2>/dev/null
}

# ATA: every status ataprint.cpp prints, by the high nibble of the
# self-test status byte, with the exit status smartctl 7.5 gives a log whose
# row "# 1" it is: bit 7 for the failures it counts (0x3-0x8), else 0.
n=0
while IFS='|' read -r status code want_rc verdict advice; do
    n=$((n + 1))
    rem=9 lba=-
    [[ $status == "Completed without error" ]] && rem=0
    [[ $status == "Completed: read failure" ]] && lba=123456
    fixture "ata-$n" "$(ata_log "$(ata_row 1 'Extended offline' "$status" "$rem" "$lba")")"
    check "ATA '$status', smartctl exit $code: $verdict, advice $advice" \
        "$(gate "S1:ata-$n:$code")|$(said S1)|$(advised S1)" "$want_rc|$verdict — $status|${ADVICE[$advice]}"
done <<'EOF'
Completed without error|0|0|PASSED|none
Aborted by host|0|1|NOT PASSED|unfinished
Interrupted (host reset)|0|1|NOT PASSED|unfinished
Fatal or unknown error|128|1|NOT PASSED|failed
Completed: unknown failure|128|1|NOT PASSED|failed
Completed: electrical failure|128|1|NOT PASSED|failed
Completed: servo/seek failure|128|1|NOT PASSED|failed
Completed: read failure|128|1|NOT PASSED|failed
Completed: handling damage??|128|1|NOT PASSED|failed
Unknown status (0x9)|0|1|NOT PASSED|unrecognised
Self-test routine in progress|0|1|STILL RUNNING|running
EOF

# SCSI: every result scsiprint.cpp prints, 25 wide as it pads them (result 7
# is "Failed in segment" and its " -->    "), with smartctl 7.5's exit
# status for it: bit 2 for result 3, bit 7 for results 4-7, else 0. Only a
# whole "Completed" field passes; "Completed, segment failed" shares its
# first word.
while IFS='|' read -r result seg lba sense code want_rc verdict shown advice; do
    n=$((n + 1))
    hours=1234
    [[ $result == "Self test in progress ..." ]] && hours=NOW
    fixture "scsi-$n" "$(scsi_log "$(scsi_row 1 'Background long ' "$result" "$seg" "$hours" "$lba" "$sense")")"
    check "SCSI '$shown', smartctl exit $code: $verdict, advice $advice" \
        "$(gate "S1:scsi-$n:$code")|$(said S1)|$(advised S1)" "$want_rc|$verdict — $shown|${ADVICE[$advice]}"
done <<'EOF'
Completed                |-|-|-|0|0|PASSED|Completed|none
Aborted (by user command)|-|-|-|0|1|NOT PASSED|Aborted (by user command)|unfinished
Aborted (device reset ?) |-|-|-|0|1|NOT PASSED|Aborted (device reset ?)|unfinished
Unknown error, incomplete|-|-|-|4|1|NOT PASSED|Unknown error, incomplete|failed
Completed, segment failed|-|1234567|0x3 0x11 0x0|128|1|NOT PASSED|Completed, segment failed|failed
Failed in first segment  |1|1234567|0x3 0x11 0x0|128|1|NOT PASSED|Failed in first segment|failed
Failed in second segment |2|1234567|0x3 0x11 0x0|128|1|NOT PASSED|Failed in second segment|failed
Failed in segment -->    |3|1234567|0x3 0x11 0x0|128|1|NOT PASSED|Failed in segment -->|failed
Reserved(8)              |-|-|-|0|1|NOT PASSED|Reserved(8)|unrecognised
Reserved(9)              |-|-|-|0|1|NOT PASSED|Reserved(9)|unrecognised
Reserved(10)             |-|-|-|0|1|NOT PASSED|Reserved(10)|unrecognised
Reserved(11)             |-|-|-|0|1|NOT PASSED|Reserved(11)|unrecognised
Reserved(12)             |-|-|-|0|1|NOT PASSED|Reserved(12)|unrecognised
Reserved(13)             |-|-|-|0|1|NOT PASSED|Reserved(13)|unrecognised
Reserved(14)             |-|-|-|0|1|NOT PASSED|Reserved(14)|unrecognised
Self test in progress ...|-|-|-|0|1|STILL RUNNING|Self test in progress ...|running
EOF
SCSI_PASSED="$(scsi_row 1 'Background long ' 'Completed                ' - 1234 - -)"
fixture scsi-passed "$(scsi_log "$SCSI_PASSED")"
fixture scsi-running "$(scsi_log "$(scsi_row 1 'Background short' 'Self test in progress ...' - NOW - -)")"
fixture scsi-aborted "$(scsi_log "$(scsi_row 1 'Background short' 'Aborted (by user command)' - 1234 - -)")"

# Never run: the log is empty, and each prints so in its own words.
fixture ata-none "$(printf '%s\n' "$BANNER" '=== START OF READ SMART DATA SECTION ===' \
    'SMART Self-test log structure revision number 1' \
    'No self-tests have been logged.  [To run self-tests, use: smartctl -t]')"
check "ATA, no self-test ever run: NOT PASSED" "$(gate S1:ata-none:0)|$(said S1)" \
    "1|NOT PASSED — no self-test logged"
fixture scsi-none "$(printf '%s\n' "$BANNER" '=== START OF READ SMART DATA SECTION ===' \
    'No Self-tests have been logged')"
check "SCSI, no self-test ever run: NOT PASSED" "$(gate S1:scsi-none:0)|$(said S1)" \
    "1|NOT PASSED — no self-test logged"

# The exit status, read apart from the output (bd jgzx), over a row that
# passed: only bits 2 and 6 leave the pass standing.
ATA_PASSED="$(ata_row 1 'Extended offline' 'Completed without error' 0 -)"
fixture ata-passed "$(ata_log "$ATA_PASSED")"
for code in 0 4 64 68; do
    check "a passed row, smartctl exit $code: PASSED" \
        "$(gate "S1:ata-passed:$code")|$(said S1)" "0|PASSED — Completed without error"
done
# Bits 0-1, smartctl could not read the drive — even beside a row that reads
# as a pass, as a stub may print. 127: no smartctl; 143: killed by SIGTERM.
for code in 1 2 3 127 143; do
    check "a passed row, smartctl exit $code: unreadable, NOT PASSED" \
        "$(gate "S1:ata-passed:$code")|$(said S1)" "1|NOT PASSED — smartctl could not read it (exit $code)"
done
check "a passed row, smartctl exit 128 (bit 7): NOT PASSED" \
    "$(gate S1:ata-passed:128)|$(said S1)" \
    "1|NOT PASSED — Completed without error, but the log records a failed self-test (smartctl exit 128)"
# Not every signal sets bits 0-1 — SIGILL is 132, bits 2 and 7 — but every
# signal sets bit 7, so none can let a pass through.
check "a passed row, smartctl exit 132 (bits 2 and 7, or SIGILL): NOT PASSED" \
    "$(gate S1:ata-passed:132)|$(said S1)" \
    "1|NOT PASSED — Completed without error, but the log records a failed self-test (smartctl exit 132)"
# Bits 3-5 come only with -H, never with -l selftest (ataprint.cpp,
# scsiprint.cpp, 7.5). Were one ever set, it says the drive is failing.
for code in 8 16 32; do
    check "a passed row, smartctl exit $code: NOT PASSED" \
        "$(gate "S1:ata-passed:$code")|$(said S1)" \
        "1|NOT PASSED — Completed without error, but smartctl reports a problem (exit $code)"
done
fixture open-failed "$BANNER"$'\n''Smartctl open device: /dev/sdz failed: No such device'
check "smartctl could not open the drive (exit 2): NOT PASSED" "$(gate S1:open-failed:2)|$(said S1)" \
    "1|NOT PASSED — smartctl could not read it (exit 2)"
fixture usb-bridge "$BANNER"$'\n''/dev/sdz: Unknown USB bridge [0x152d:0x0578 (0x214)]'$'\n'\
'Please specify device type with the -d option.'$'\n\n''Use smartctl -h to get a usage summary'
check "smartctl does not know the USB bridge (exit 1): NOT PASSED" "$(gate S1:usb-bridge:1)|$(said S1)" \
    "1|NOT PASSED — smartctl could not read it (exit 1)"
# A failed row beside bit 6 used to get a second status, "No tests", from
# `|| echo "No tests"` under pipefail (jgzx).
fixture ata-read-failure "$(ata_log "$(ata_row 1 'Extended offline' 'Completed: read failure' 9 123456)")"
check "a failed row, smartctl exit 64: the row's own status, NOT PASSED" \
    "$(gate S1:ata-read-failure:64)|$(said S1)" "1|NOT PASSED — Completed: read failure"
check "a failed row, smartctl exit 64: no 'No tests' added" "$(grep -c 'No tests' "$WORK/gate.out")" "0"
# The field rule holds by itself, not only behind bit 7: a failure printed
# without the bit (a stub, another smartctl, a bridge) still does not pass,
# though it shares its first word with a pass.
check "ATA 'Completed: read failure' without bit 7 (exit 0): NOT PASSED" \
    "$(gate S1:ata-read-failure:0)|$(said S1)" "1|NOT PASSED — Completed: read failure"
fixture scsi-segment-failed "$(scsi_log "$(scsi_row 1 'Background long ' 'Completed, segment failed' - 1234 1234567 '0x3 0x11 0x0')")"
check "SCSI 'Completed, segment failed' without bit 7 (exit 0): NOT PASSED" \
    "$(gate S1:scsi-segment-failed:0)|$(said S1)" "1|NOT PASSED — Completed, segment failed"
# No row at all, with a status that alone would pass: no reading is no pass.
for no_row in 'Read SMART Self-test Log failed: scsi error aborted command|4' \
    'SMART Self-test Log not supported|0' '|0'; do
    fixture no-row "${no_row%|*}"
    check "no row ('${no_row%|*}'), smartctl exit ${no_row#*|}: NOT PASSED" \
        "$(gate "S1:no-row:${no_row#*|}")|$(said S1)" \
        "1|NOT PASSED — no self-test result in smartctl's output (exit ${no_row#*|})"
done
# An invalid SMART checksum (review F1). By default (-b warn) smartctl warns
# and carries on: the rows print at exit 0, so a log it calls invalid would
# pass. The gate reads with -b exit, where smartctl stops at that warning,
# exit 4, no row. Both behaviours, and these fixtures byte for byte, were
# measured on the smartctl 7.5 binary itself, run on a replayed drive (its
# "-" device reads a "-r ataioctl,2" dump from stdin and touches no drive).
for structure in "SMART Self-Test Log Structure" "Drive Identity Structure" "SMART Attribute Data Structure"; do
    fixture "cksum-${structure// /-}" "$(ata_log_bad_checksum "$structure" "$ATA_PASSED")"
    check "a passed row, invalid checksum in the $structure: NOT PASSED" \
        "$(gate "S1:cksum-${structure// /-}:0")|$(said S1)" \
        "1|NOT PASSED — no self-test result: smartctl found an invalid checksum in the $structure (exit 4)"
done
check "the gate asks smartctl with -b exit" "$(grep -c '^-b exit -l selftest ' "$WORK/smartctl.calls")" "1"
# What smartctl -b exit prints for an invalid IDENTIFY checksum: the banner and
# its warning, nothing more (exit 4).
fixture cksum-exit-identity "$(printf '%s\n' "$BANNER" 'Warning! Drive Identity Structure error: invalid SMART checksum.')"
# What -b exit is for: that log as smartctl prints it by default (the 2.3.0
# call) reaches the decision with its row, and the row passes.
check "the same log without -b exit, as 2.3.0 read it: its row passes the decision" \
    "$(decide cksum-SMART-Self-Test-Log-Structure 0)" "0|Completed without error"

# Only row "# 1", the most recent, decides; the rows below it only through
# smartctl's bit 7. ATA counts a failure outdated by a newer passed EXTENDED
# test, and says so on a line of its own that ends in "# 1"; SCSI counts any
# failure among its 20.
fixture ata-older-outdated "$(ata_log "$ATA_PASSED"$'\n'"$(ata_row 2 'Short offline' 'Completed: read failure' 9 123456)")
1 of 1 failed self-tests are outdated by newer successful extended offline self-test # 1"
check "ATA: # 1 extended passed, an older failure outdated (exit 0): PASSED" \
    "$(gate S1:ata-older-outdated:0)|$(said S1)" "0|PASSED — Completed without error"
fixture ata-older-failed "$(ata_log "$(ata_row 1 'Short offline' 'Completed without error' 0 -)"$'\n'"$(
    ata_row 2 'Extended offline' 'Completed: read failure' 9 123456)")"
check "ATA: # 1 short passed, an older extended failed (exit 128): NOT PASSED" \
    "$(gate S1:ata-older-failed:128)|$(said S1)" \
    "1|NOT PASSED — Completed without error, but the log records a failed self-test (smartctl exit 128)"
fixture ata-newest-failed "$(ata_log "$(ata_row 1 'Extended offline' 'Completed: read failure' 9 123456)"$'\n'"$(
    ata_row 2 'Extended offline' 'Completed without error' 0 -)")"
check "ATA: # 1 failed, an older one passed (exit 128): NOT PASSED" \
    "$(gate S1:ata-newest-failed:128)|$(said S1)" "1|NOT PASSED — Completed: read failure"
fixture scsi-older-incomplete "$(scsi_log "$SCSI_PASSED"$'\n'"$(
    scsi_row 2 'Background short' 'Unknown error, incomplete' - 1200 - -)")"
check "SCSI: # 1 passed, an older one incomplete (exit 4): PASSED" \
    "$(gate S1:scsi-older-incomplete:4)|$(said S1)" "0|PASSED — Completed"
fixture scsi-older-failed "$(scsi_log "$SCSI_PASSED"$'\n'"$(
    scsi_row 2 'Background long ' 'Failed in first segment  ' 1 1200 1234567 '0x3 0x11 0x0')")"
check "SCSI: # 1 passed, an older one failed (exit 128): NOT PASSED" \
    "$(gate S1:scsi-older-failed:128)|$(said S1)" \
    "1|NOT PASSED — Completed, but the log records a failed self-test (smartctl exit 128)"

# What to do, for each reason that is not a row's own status (the tables
# above cover those), and nothing for a drive that passed.
fixture log-not-supported "$BANNER"$'\n''=== START OF READ SMART DATA SECTION ==='$'\n''SMART Self-test Log not supported'
for case in ata-none:0:never-run scsi-none:0:never-run \
    open-failed:2:unreadable usb-bridge:1:unreadable ata-passed:143:unreadable \
    cksum-SMART-Self-Test-Log-Structure:0:checksum cksum-Drive-Identity-Structure:0:checksum \
    log-not-supported:0:no-result ata-passed:8:flagged \
    ata-older-failed:128:older-ata ata-passed:132:older-ata scsi-older-failed:128:older-scsi \
    ata-passed:0:none ata-older-outdated:0:none scsi-older-incomplete:4:none; do
    IFS=: read -r fx code key <<<"$case"
    gate "S1:$fx:$code" >/dev/null
    check "what to do, $fx at smartctl exit $code: $key" "$(advised S1)" "${ADVICE[$key]}"
done

# Every drive must pass; one that does not blocks them all. Each is named by
# serial, never by its device path.
fixture ata-running "$(ata_log "$(ata_row 1 'Extended offline' 'Self-test routine in progress' 9 -)")"
check "two drives, both passed: the gate passes" \
    "$(gate S1:ata-passed:0 S2:ata-passed:0)|$(said S1)|$(said S2)" \
    "0|PASSED — Completed without error|PASSED — Completed without error"
check "two drives, both passed: says so" \
    "$(grep -c '^\[INFO\] Every drive PASSED its most recent SMART self-test$' "$WORK/gate.out")" "1"
check "two drives, one failed: the gate blocks" \
    "$(gate S1:ata-passed:0 S2:ata-read-failure:128)|$(said S1)|$(said S2)" \
    "1|PASSED — Completed without error|NOT PASSED — Completed: read failure"
check "two drives, one failed: says so" \
    "$(grep -c '^\[WARN\] Not every drive has PASSED its most recent SMART self-test\.$' "$WORK/gate.out")" "1"
check "two drives, one still running: the gate blocks" \
    "$(gate S1:ata-passed:0 S2:ata-running:0)|$(said S2)" "1|STILL RUNNING — Self-test routine in progress"
check "two drives, one unreadable: the gate blocks" \
    "$(gate S1:open-failed:2 S2:ata-passed:0)|$(said S1)" "1|NOT PASSED — smartctl could not read it (exit 2)"
check "three drives, one aborted: the gate blocks" \
    "$(gate S1:ata-passed:0 S2:scsi-aborted:0 S3:ata-passed:0)|$(said S2)" "1|NOT PASSED — Aborted (by user command)"
check "three drives: none named by device path" "$(grep -c 'not-a-device' "$WORK/gate.out")" "0"
check "three drives: smartctl asked once each, for the self-test log, with -b exit" \
    "$(grep -c '^-b exit -l selftest .*/not-a-device-S[123]$' "$WORK/smartctl.calls")" "3"

# Output over 64 KiB, its row on line 1 or after it all. The filler lines
# start "# 1 " and one space: no row of smartctl's starts so.
filler_lines() { # filler_lines: 1500 lines, none of them a row
    local i
    for ((i = 0; i < 1500; i++)); do printf '# 1 filler line %06d of the self-test log, nothing to see\n' "$i"; done
}
for kind in ata-passed ata-running ata-read-failure scsi-running; do
    row="$(grep -m1 '^# 1  ' "$WORK/st-$kind.txt")"
    { echo "$row" && filler_lines; } >"$WORK/st-big-first-$kind.txt"
    big_size_ok "$WORK/st-big-first-$kind.txt"
    { filler_lines && echo "$row"; } >"$WORK/st-big-last-$kind.txt"
    big_size_ok "$WORK/st-big-last-$kind.txt"
done
for at in first last; do
    check "row on the $at line of output over 64 KiB, passed: PASSED" \
        "$(gate "S1:big-$at-ata-passed:0")|$(said S1)" "0|PASSED — Completed without error"
    check "row on the $at line of output over 64 KiB, running: STILL RUNNING" \
        "$(gate "S1:big-$at-ata-running:0")|$(said S1)" "1|STILL RUNNING — Self-test routine in progress"
    check "row on the $at line of output over 64 KiB, SCSI running: STILL RUNNING" \
        "$(gate "S1:big-$at-scsi-running:0")|$(said S1)" "1|STILL RUNNING — Self test in progress ..."
    check "row on the $at line of output over 64 KiB, failed: NOT PASSED" \
        "$(gate "S1:big-$at-ata-read-failure:128")|$(said S1)" "1|NOT PASSED — Completed: read failure"
done

# With no fd to spare (ulimit -n 3). The decision alone needs none: it reads
# the output it is handed by bash itself, no pipe and no file (round 4, N3);
# one that needed a fd would read "no match" here, and a pass would block.
check "no fd to spare: a passed row still passes" "$(decide ata-passed 0 no-fd)" "0|Completed without error"
check "no fd to spare: a passed SCSI row still passes" "$(decide scsi-passed 0 no-fd)" "0|Completed"
check "no fd to spare: a passed row after 64 KiB still passes" \
    "$(decide big-last-ata-passed 0 no-fd)" "0|Completed without error"
check "no fd to spare: a failed row still fails" "$(decide ata-read-failure 128 no-fd)" "1|Completed: read failure"
check "no fd to spare: a running SCSI row still fails" "$(decide scsi-running 0 no-fd)" "1|Self test in progress ..."
check "no fd to spare: an invalid checksum still fails, and says so" \
    "$(decide cksum-exit-identity 4 no-fd)" \
    "1|no self-test result: smartctl found an invalid checksum in the Drive Identity Structure (exit 4)"
# The whole gate with no fd to spare cannot capture smartctl at all. bash
# says "cannot make pipe for command substitution", and the capture returns
# 0 with nothing in it (measured, bash 5.3): no reading, so no pass.
gate_no_fd() { # gate_no_fd <serial>:<fixture>:<status>: its output, then "rc=<returned>"
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/check_smart_tests.sh"
        gate_drives "$@"
        ulimit -n 3
        rc=0
        check_smart_tests || rc=$?
        echo "rc=$rc"
    ) 2>/dev/null
}
no_fd_out="$(gate_no_fd S1:ata-passed:0)"
check "no fd to spare, the whole gate over a passed drive: blocks" "${no_fd_out##*$'\n'}" "rc=1"
check "no fd to spare, the whole gate: says why" \
    "$(grep -F '  S1 (label-S1): ' <<<"$no_fd_out")" \
    "  S1 (label-S1): NOT PASSED — no self-test result in smartctl's output (exit 0)"

# main(), around the real gate: YES-DESTROY is reached only when every drive
# passed. Each step around the gate is a stub that records it was reached;
# nothing partitions and nothing reads a device.
run_main() { # run_main <mode> <serial>:<fixture>:<status>...: "<exit status>|<steps reached>"
    local mode="$1"
    shift
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/check_smart_tests.sh"
        # shellcheck source=/dev/null
        source "$WORK/main.sh"
        gate_drives "$@"
        date() { echo 2026-10-04; }
        check_root() { :; }
        discover_devices() { :; }
        verify_serials() { :; }
        verify_esp_labels_unique() { :; }
        show_plan() { echo "REACHED show_plan"; }
        confirm_destruction() { echo "REACHED YES-DESTROY"; }
        run_partitioning() { echo "REACHED run_partitioning"; }
        rc=0
        (main "$mode") >"$WORK/main.out" 2>&1 || rc=$?
        reached=""
        while IFS= read -r line; do
            if [[ $line == "REACHED "* ]]; then reached+="${line#REACHED } "; fi
        done <"$WORK/main.out"
        echo "$rc|${reached% }"
    )
}
check "main --run, every drive passed: on to YES-DESTROY" \
    "$(run_main --run S1:ata-passed:0 S2:scsi-passed:0)" "0|show_plan YES-DESTROY run_partitioning"
check "main --run, one drive failed: exits 1 before the plan and YES-DESTROY" \
    "$(run_main --run S1:ata-passed:0 S2:ata-read-failure:128)" "1|"
check "main --run, one drive failed: says partitioning is blocked" \
    "$(grep -c '^\[ERROR\] Partitioning blocked: every drive must have PASSED its most recent SMART self-test' "$WORK/main.out")" "1"
# The advice follows each drive's reason, under that drive; the error line
# points to it and gives no reason of its own (round 3).
check "main --run, one drive failed: what to do is under that drive" \
    "$(grep -cF "      → ${ADVICE[failed]}" "$WORK/main.out")" "1"
check "main --run: the error line points to the advice, and to --force as deliberate" \
    "$(grep -c '^\[ERROR\] What to do is under each drive that did not\. --force skips this check: use it only deliberately\.$' "$WORK/main.out")" "1"
check "main --run: no advice that ignores the reason" \
    "$(grep -c "where none passed\|replace a drive whose test failed" "$WORK/main.out")" "0"
check "main --run, one drive never tested: exits 1 before YES-DESTROY" \
    "$(run_main --run S1:ata-passed:0 S2:ata-none:0)" "1|"
check "main --run, one drive never tested: told to run an extended test" \
    "$(grep -cF "      → ${ADVICE[never-run]}" "$WORK/main.out")" "1"
check "main --check, one drive failed: the plan, never YES-DESTROY" \
    "$(run_main --check S1:ata-passed:0 S2:ata-read-failure:128)" "0|show_plan"
check "main --check, one drive failed: names it" \
    "$(grep -cF '  S2 (label-S2): NOT PASSED — Completed: read failure' "$WORK/main.out")" "1"
check "main --check, one drive failed: and says what to do" \
    "$(grep -cF "      → ${ADVICE[failed]}" "$WORK/main.out")" "1"
# --force is the one way past a failed drive, as its usage says: it skips
# the gate, and smartctl is never asked.
check "main --force, one drive failed: on to YES-DESTROY" \
    "$(run_main --force S1:ata-passed:0 S2:ata-read-failure:128)" "0|show_plan YES-DESTROY run_partitioning"
check "main --force: smartctl never asked" "$(grep -c . "$WORK/smartctl.calls")" "0"
# An invalid checksum blocks --run like any other drive that has not passed;
# --force still overrides it, and still asks smartctl nothing.
check "main --run, a self-test log with an invalid checksum: exits 1 before YES-DESTROY" \
    "$(run_main --run S1:ata-passed:0 S2:cksum-SMART-Self-Test-Log-Structure:0)" "1|"
check "main --force, the same drive: on to YES-DESTROY" \
    "$(run_main --force S1:ata-passed:0 S2:cksum-SMART-Self-Test-Log-Structure:0)" "0|show_plan YES-DESTROY run_partitioning"
check "main --force over it: smartctl never asked" "$(grep -c . "$WORK/smartctl.calls")" "0"

# ---------------------------------------------------------------------------
echo "== each site's condition, as its script has it, with no fd to spare"
# ---------------------------------------------------------------------------
# A here-string needs a file or a pipe: bash writes one over 64 KiB to a temp
# file and sends a smaller one down a pipe. With no fd left (ulimit -n 3) it
# can make neither, says "cannot create temp file for here-document", and
# the `if` reads "no match" (round 4, N3; measured for every site). `[[ ]]`
# needs no fd. Each condition is taken from its script. The self-test gate's
# decision and its capture are tested with no fd to spare in its own section.
condition() { # condition <script> <ERE matching one if/elif line>: its condition
    local line
    line="$(grep -E -- "$2" "$ROOT/scripts/$1")" || harness_broken "no line in $1 matches: $2"
    [[ "$line" != *$'\n'* ]] || harness_broken "more than one line in $1 matches: $2"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line#el}"
    line="${line#if }"
    printf '%s\n' "${line%; then}"
}
with_no_fd_to_spare() { # with_no_fd_to_spare <condition> <variable> <value>
    (
        set -euo pipefail
        printf -v "$2" '%s' "$3"
        ulimit -n 3
        if eval "$1"; then echo match; else echo "no match"; fi
    ) 2>/dev/null
}
check "no fd to spare: the drift check still sees a btrbk name" \
    "$(with_no_fd_to_spare "$(condition backup-run.sh '^[[:space:]]*if .*\$subvol_listing')" \
        subvol_listing "$(cat "$WORK/drift-big.txt")")" "match"
check "no fd to spare: the SMART check still sees PASSED" \
    "$(with_no_fd_to_spare "$(condition backup-verify.sh '^[[:space:]]*if .*PASSED')" \
        health "$PASSED_LINE")" "match"

# ---------------------------------------------------------------------------
echo "== the drift check's digits are ASCII digits, whatever the locale"
# ---------------------------------------------------------------------------
# grep's [0-9] matched ASCII digits only. Under en_US.UTF-8 — the host's
# locale — bash's regex [0-9] also matches Arabic-Indic and fullwidth digits,
# so a name like root.٢٠٢٦١٠٠٤T٠٣٠٠ read as btrbk-shaped (round 5, N5;
# measured). [[:digit:]] is ASCII only: the old meaning exactly. Such a name
# is no btrbk snapshot, so its target takes the quiet skip, as it did.
NONASCII_ARABIC='ID 301 gen 9 top level 5 path nvme/renamed-root.٢٠٢٦١٠٠٤T٠٣٠٠'
NONASCII_FULLWIDTH='ID 302 gen 9 top level 5 path nvme/renamed-root.２０２６１００４T０３００'
# Only where the locale shows the difference: bash's own [0-9] must match a
# non-ASCII digit there, or these checks could not fail.
not_run=""
probe_digit='٢'
if (export LC_ALL=en_US.UTF-8; [[ $probe_digit =~ [0-9] ]]) 2>/dev/null; then
    for name in NONASCII_ARABIC NONASCII_FULLWIDTH; do
        printf '%s\n' "${!name}" >"$WORK/$name.txt"
        check "en_US.UTF-8, the only btrbk-like name has non-ASCII digits ($name): the quiet skip" \
            "$(BOOT_LOCALE=en_US.UTF-8 run_boot_subvols "$WORK/$name.txt")" "OK|0 updated, 1 skipped"
    done
    check "en_US.UTF-8, an ASCII drifted name: still FAIL" \
        "$(BOOT_LOCALE=en_US.UTF-8 run_boot_subvols "$WORK/drift-small.txt")" "FAIL|0 updated, 1 failed"
    drift_cond="$(condition backup-run.sh '^[[:space:]]*if .*\$subvol_listing')"
    check "en_US.UTF-8: the drift condition on Arabic-Indic digits: no match" \
        "$(export LC_ALL=en_US.UTF-8; subvol_listing="$NONASCII_ARABIC"; if eval "$drift_cond"; then echo match; else echo "no match"; fi)" "no match"
else
    not_run="the en_US.UTF-8 cases: bash's [0-9] matches no non-ASCII digit here (locale missing?)"
    echo "NOT RUN: $not_run"
fi

# ---------------------------------------------------------------------------
echo "== no producer | grep -q left in scripts/"
# ---------------------------------------------------------------------------
# Every `if`/`elif` that pipes into grep -q under these scripts' pipefail is
# the shape above. None may come back; a comment may still quote one.
left="$(awk '!/^[[:space:]]*#/ && /\|[[:space:]]*grep([[:space:]]+-[a-zA-Z]+)*[[:space:]]+-[a-zA-Z]*q/ { print FILENAME ":" FNR ": " $0 }' "$ROOT"/scripts/*.sh)"
check "no 'producer | grep -q' in scripts/" "${left:-none}" "none"

echo ""
echo "passed=$pass failed=$fail"
[[ -z "$not_run" ]] || echo "NOT RUN: $not_run"
if ((fail == 0)); then
    echo "EARLY-EXIT READERS SUITE GREEN"
    exit 0
fi
exit 1
