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
#   backup-run.sh          update_boot_subvolumes  the match against a
#                          target's subvolume listing (exposed: the primary
#                          target's listing is near 64 KiB and grows), now
#                          latest_boot_snapshot's walk by word split
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
echo "== backup-run.sh: update_boot_subvolumes, the listing walk and the boot plan"
# ---------------------------------------------------------------------------
# The names come from `btrdasd backup boot-plan` (btrbk.conf), not from
# patterns in the script, so a target whose listing holds only OTHER series
# is a WARN for the subvolume that has none — there is no "drift" to detect
# any more (dtm). The listing is walked by bash itself, never piped.
extract backup-run.sh update_boot_subvolumes 'backup boot-plan'
extract backup-run.sh update_boot_subvol 'mv -T --'
extract backup-run.sh latest_boot_snapshot 'LATEST_BOOT_SNAPSHOT'
extract backup-run.sh boot_path_state 'stat'
extract backup-run.sh record_op 'OP_STATUS[$op]="$result"'
extract backup-run.sh probe_mount_point 'LC_ALL=C mountpoint'
extract backup-run.sh probe_state 'probe_mount_point "$1"'

# The mount points of a run, and what mountpoint says about each (BOOT_MOUNTS,
# BOOT_PROBES: "<mount>=<answer> ..."; a mount point not named is mounted).
# The stub answers as util-linux 2.42.4 does (measured): 0 a mount point, 32
# not one, 1 an error — and 1, "No such file or directory", for a path that is
# not there. BOOT_NO_MOUNTPOINT=1 leaves the program out of PATH altogether
# (exit 127), with the few tools the function needs.
mkdir -p "$WORK/boot-path"
for tool in awk cat date grep mktemp rm sort tail tr; do
    ln -s "$(type -P "$tool")" "$WORK/boot-path/$tool" || harness_broken "no $tool on this system"
done

run_boot_subvols() { # run_boot_subvols <listing file> [tmp-full]: "<result>|<detail>"
    : >"$WORK/btrfs.calls"
    : >"$WORK/btrdasd.calls"
    : >"$WORK/mktemp.calls"
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/update_boot_subvolumes.sh"
        # shellcheck source=/dev/null
        source "$WORK/record_op.sh"
        # shellcheck source=/dev/null
        source "$WORK/probe_mount_point.sh"
        # shellcheck source=/dev/null
        source "$WORK/probe_state.sh"
        # shellcheck source=/dev/null
        source "$WORK/latest_boot_snapshot.sh"
        # shellcheck source=/dev/null
        source "$WORK/boot_path_state.sh"
        # shellcheck source=/dev/null
        source "$WORK/update_boot_subvol.sh"
        LISTING="$1"
        BTRDASD_BIN=btrdasd
        DAS_CONFIG=/etc/das-backup/config.toml
        DAS_BOOT_ENABLED="${BOOT_ENABLED:-true}"
        # The plan `btrdasd backup boot-plan` prints (BOOT_PLAN, tab-separated
        # lines), or its failure (BOOT_PLAN_RC) with the reason on stderr.
        btrdasd() {
            printf '%s\n' "$*" >>"$WORK/btrdasd.calls"
            if [[ "${BOOT_PLAN_RC:-0}" != 0 ]]; then
                echo "error: cannot read /nonexistent/btrbk.conf (stub)" >&2
                return "$BOOT_PLAN_RC"
            fi
            printf '%b' "${BOOT_PLAN-@\troot-\tnvme\n@home\thome\tnvme\n}"
        }
        # stat as the helper asks it (`-c %F -- <path>`): a path in
        # BOOT_PRESENT exists (a directory), one in BOOT_SYMLINK is a symbolic
        # link, one in BOOT_STAT_ERR cannot be told, anything else is not there.
        # Each entry is a glob (the archive's name carries the run's clock).
        stat() {
            local p="${!#}" q
            for q in ${BOOT_PRESENT:-}; do
                # shellcheck disable=SC2053  # the patterns are globs, on purpose
                if [[ "$p" == $q ]]; then
                    echo directory
                    return 0
                fi
            done
            for q in ${BOOT_SYMLINK:-}; do
                # shellcheck disable=SC2053
                if [[ "$p" == $q ]]; then
                    echo "symbolic link"
                    return 0
                fi
            done
            for q in ${BOOT_STAT_ERR:-}; do
                # shellcheck disable=SC2053
                if [[ "$p" == $q ]]; then
                    echo "stat: cannot statx '$p': Input/output error" >&2
                    return 1
                fi
            done
            echo "stat: cannot statx '$p': No such file or directory" >&2
            return 1
        }
        # mv is only ever the swap of a staging subvolume into place.
        mv() {
            printf 'mv %s\n' "$*" >>"$WORK/btrfs.calls"
            [[ "${BOOT_MV_FAILS:-}" != 1 ]]
        }
        # BOOT_MKTEMP_FAIL_NTH: the Nth mktemp of the step fails (1 = the
        # boot plan's error file, 2 = the first target's listing capture).
        # The count lives in a file: each call runs in a command substitution.
        mktemp() {
            local n=0
            if [[ -n "${BOOT_MKTEMP_FAIL_NTH:-}" ]]; then
                echo x >>"$WORK/mktemp.calls"
                while read -r _; do ((n += 1)); done <"$WORK/mktemp.calls"
                if ((n == BOOT_MKTEMP_FAIL_NTH)); then
                    echo "mktemp: failed to create file (stub)" >&2
                    return 1
                fi
            fi
            command mktemp "$@"
        }
        declare -A OP_STATUS=()
        declare -A MOUNT_ROLES=() TARGET_MOUNTS=()
        read -r -a ALL_TARGET_MOUNTS <<<"${BOOT_MOUNTS:-/mnt/t}"
        for m in "${ALL_TARGET_MOUNTS[@]}"; do
            MOUNT_ROLES[$m]=primary
            TARGET_MOUNTS["label-${m##*/}"]=$m
        done
        # BOOT_MIRRORS: the mount points among them that are mirrors.
        for m in ${BOOT_MIRRORS:-}; do
            MOUNT_ROLES[$m]=mirror
        done
        mountpoint() {
            local p="${!#}" pair
            for pair in ${BOOT_PROBES:-}; do
                [[ "${pair%%=*}" == "$p" ]] || continue
                case "${pair#*=}" in
                notmounted) return 32 ;;
                absent)
                    echo "mountpoint: $p: No such file or directory" >&2
                    return 1
                    ;;
                error)
                    echo "mountpoint: $p: Input/output error" >&2
                    return 1
                    ;;
                esac
            done
            return 0
        }
        btrfs() {
            printf '%s\n' "$*" >>"$WORK/btrfs.calls"
            case "$1 $2" in
            "filesystem label") echo das-backup-test ;;
            "subvolume list")
                # BOOT_LIST_RC: the listing fails, with its reason on stderr.
                if [[ -n "${BOOT_LIST_RC:-}" ]]; then
                    echo "ERROR: can't access '$3': Input/output error (stub)" >&2
                    return "$BOOT_LIST_RC"
                fi
                # BOOT_LIST_VANISH: the capture itself dies before btrfs's
                # status can be printed (as when bash cannot make it): the
                # stub's subshell exits, so the capture holds no status line.
                [[ -z "${BOOT_LIST_VANISH:-}" ]] || exit 0
                cat "$LISTING"
                ;;
            "subvolume snapshot")
                # BOOT_FAIL_ON: a substring of the call that fails.
                if [[ -n "${BOOT_FAIL_ON:-}" && "$*" == *"$BOOT_FAIL_ON"* ]]; then
                    echo "ERROR: stub refuses: $*" >&2
                    return 1
                fi
                ;;
            "subvolume delete")
                if [[ -n "${BOOT_FAIL_ON:-}" && "$*" == *"$BOOT_FAIL_ON"* ]]; then
                    echo "ERROR: stub refuses: $*" >&2
                    return 1
                fi
                ;;
            *)
                echo "btrfs stub: unexpected: $*" >&2
                return 99
                ;;
            esac
        }
        log_info() { echo "[INFO] $*"; }
        log_warn() { echo "[WARN] $*"; }
        log_error() { echo "[ERROR] $*"; }
        # A probe that is itself broken (BOOT_PROBE_STUB): it prints nothing,
        # as when bash could not make its capture, or something nobody
        # recognises (bd DAS-Backup-Manager-hhow).
        case "${BOOT_PROBE_STUB:-}" in
        silent) probe_mount_point() { :; } ;;
        garbage) probe_mount_point() { echo banana; } ;;
        esac
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
        if [[ -n "${BOOT_NO_MOUNTPOINT:-}" ]]; then
            unset -f mountpoint
            # shellcheck disable=SC2123  # the point: a PATH with no mountpoint on it
            PATH="$WORK/boot-path"
        fi
        if [[ -n "${BOOT_LIMIT:-}" ]]; then
            # Descriptor starvation (bd DAS-Backup-Manager-hhow): output goes
            # to a file first, then the limit is lowered, and it is raised
            # again only to print the result (the soft limit only: a hard
            # limit, once lowered, cannot be raised). Nothing in between needs
            # a descriptor the test did not give it. The result is a line of
            # boot.out: boot_at_limit reads it.
            limit_before="$(ulimit -S -n)"
            exec >"$WORK/boot.out" 2>&1
            ulimit -S -n "$BOOT_LIMIT"
            update_boot_subvolumes "${BOOT_FORCE:-true}" || true
            ulimit -S -n "$limit_before"
            printf 'RESULT %s|%s\n' "${OP_STATUS[boot_subvols]:-unset}" "${OP_STATUS[boot_subvols_detail]:-}"
            exit 0
        fi
        update_boot_subvolumes "${BOOT_FORCE:-true}" >"$WORK/boot.out" 2>&1
        printf '%s|%s\n' "${OP_STATUS[boot_subvols]:-unset}" "${OP_STATUS[boot_subvols_detail]:-}"
    )
}
DRIFTED='ID 300 gen 9 top level 5 path nvme/renamed-root.20261004T0300'

{
    echo "$DRIFTED"
    filler 1500
} >"$WORK/drift-big.txt"
big_size_ok "$WORK/drift-big.txt"
check "boot subvolumes, another series on line 1 of a listing over 64 KiB: WARN, nothing touched" \
    "$(run_boot_subvols "$WORK/drift-big.txt")" "WARN|0 updated, 0 skipped, 2 warnings"
check "boot subvolumes, another series over 64 KiB: says there is none of the planned name" \
    "$(grep -c "No btrbk snapshot named 'root-'" "$WORK/boot.out")" "1"

# The same listing with /tmp full: a here-string that large went to a temp
# file, could not be written ("here-document: No space left on device"),
# and the check read "no match" — the quiet branch again, 5 of 5 times
# measured (round 4, N3). `[[ =~ ]]` writes nothing.
check "boot subvolumes, another series over 64 KiB with /tmp full: still the same WARN" \
    "$(run_boot_subvols "$WORK/drift-big.txt" tmp-full)" "WARN|0 updated, 0 skipped, 2 warnings"

echo "$DRIFTED" >"$WORK/drift-small.txt"
check "boot subvolumes, another series in a small listing: WARN" \
    "$(run_boot_subvols "$WORK/drift-small.txt")" "WARN|0 updated, 0 skipped, 2 warnings"

filler 1500 >"$WORK/none-big.txt"
big_size_ok "$WORK/none-big.txt"
check "boot subvolumes, no btrbk snapshots in a listing over 64 KiB: WARN, nothing touched" \
    "$(run_boot_subvols "$WORK/none-big.txt")" "WARN|0 updated, 0 skipped, 2 warnings"
check "boot subvolumes, no btrbk snapshots: says it leaves each subvolume untouched" \
    "$(grep -c "No btrbk snapshot named '.*' — leaving @.* untouched" "$WORK/boot.out")" "2"

# ---------------------------------------------------------------------------
echo "== backup-run.sh: boot plan and the shared listing (bd DAS-Backup-Manager-dtm)"
# ---------------------------------------------------------------------------
boot_btrfs_calls() { # what btrfs was asked, one call per ";"
    if [[ -s "$WORK/btrfs.calls" ]]; then tr '\n' ';' <"$WORK/btrfs.calls"; else echo none; fi
}
said_boot() { grep -cF -- "$1" "$WORK/boot.out"; }
SHARED="$ROOT/tests/fixtures/boot-subvol-listing.txt"
[[ -s "$SHARED" ]] || harness_broken "no $SHARED"

# latest_boot_snapshot against the fixture the Rust library's test also reads:
# the same seven answers, one rule in two languages (R2: newest = greatest
# timestamp, ties to the greater path).
latest_of() { # latest_of <subdirs> <snapshot_name>: the answer ("" = none)
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/latest_boot_snapshot.sh"
        latest_boot_snapshot "$(cat "$SHARED")" "$2" "$1"
        printf '%s' "$LATEST_BOOT_SNAPSHOT"
    )
}
check "shared listing: nvme root- -> the _1 collision suffix wins" "$(latest_of nvme root-)" "nvme/root-.20261005T0100_1"
check "shared listing: /nvme/ root- -> slashes trimmed" "$(latest_of /nvme/ root-)" "nvme/root-.20261005T0100_1"
check "shared listing: nvme home" "$(latest_of nvme home)" "nvme/home.20261005T0100"
check "shared listing: nvme,ssd home -> ssd, the later timestamp" "$(latest_of nvme,ssd home)" "ssd/home.20261007T0100"
check "shared listing: nvme,ssd var -> the greater TIMESTAMP, not the greater path (R2)" "$(latest_of nvme,ssd var)" "nvme/var.20261009T0100"
check "shared listing: nvme a.b -> a literal prefix, aXb never matches" "$(latest_of nvme a.b)" "nvme/a.b.20261003T0100"
check "shared listing: nvme log -> none" "$(latest_of nvme log)" ""

# A big listing with the matching series appended at its END: a pipe into an
# early-exit reader dies of SIGPIPE here (bd wkvz); the walk reads it all.
{
    filler 1500
    echo 'ID 900 gen 9 top level 5 path nvme/root-.20261010T0100'
    echo 'ID 901 gen 9 top level 5 path nvme/home.20261010T0100'
} >"$WORK/series-at-end.txt"
big_size_ok "$WORK/series-at-end.txt"
MNT=/mnt/t
snap_calls() { grep -c '^subvolume snapshot' "$WORK/btrfs.calls" || true; }
boot_seq() { boot_btrfs_calls | sed 's/^[^;]*;[^;]*;//; s/archive\.[0-9T]*/archive.TS/'; } # the writes, the stamp masked
calls_of() { grep -c -- "$1" "$WORK/btrfs.calls" || true; }

check "boot, the series at the end of a listing over 64 KiB (pipefail): both are created from it" \
    "$(BOOT_FORCE=false run_boot_subvols "$WORK/series-at-end.txt") $(boot_btrfs_calls)" \
    "OK|2 updated, 0 skipped filesystem label /mnt/t;subvolume list /mnt/t;subvolume snapshot /mnt/t/nvme/root-.20261010T0100 /mnt/t/@;subvolume snapshot /mnt/t/nvme/home.20261010T0100 /mnt/t/@home;"

# 1. [boot] enabled = false: nothing is asked, nothing is touched.
check "boot disabled in config: OK, and says so" \
    "$(BOOT_ENABLED=false run_boot_subvols "$SHARED")" "OK|disabled in config"
check "boot disabled in config: btrfs and the plan never asked" \
    "$(boot_btrfs_calls) $(wc -c <"$WORK/btrdasd.calls")" "none 0"
# 2. the plan cannot be read: FAIL, nothing touched.
check "boot plan unreadable: the step FAILS, counted" \
    "$(BOOT_PLAN_RC=2 run_boot_subvols "$SHARED")" "FAIL|0 updated, 1 failed"
check "boot plan unreadable: the reason is said, btrfs never asked" \
    "$(said_boot 'Could not read the boot plan: error: cannot read /nonexistent/btrbk.conf (stub)') $(boot_btrfs_calls)" "1 none"
# 3. a plan with a name and a gap; @ absent, an incremental run.
PLAN_GAP='@\troot-\tnvme\n@home\t-\tnvme\n'
check "boot plan with one name missing, @ absent: @ created, @home a WARN" \
    "$(BOOT_PLAN=$PLAN_GAP BOOT_FORCE=false run_boot_subvols "$SHARED")" "WARN|1 updated, 0 skipped, 1 warnings"
check "boot plan, @ absent: created from the newest root- snapshot" \
    "$(calls_of 'subvolume snapshot /mnt/t/nvme/root-.20261005T0100_1 /mnt/t/@$')" "1"
check "boot plan, a name that is '-': @home never touched" "$(calls_of '@home')" "0"
check "boot plan, a name that is '-': said" "$(said_boot '@home has no snapshot_name')" "1"
# 4. the same with @ present, incremental: skipped, no snapshot call.
check "boot plan, @ present, incremental: skipped beside the WARN" \
    "$(BOOT_PRESENT="/mnt/t/@" BOOT_PLAN=$PLAN_GAP BOOT_FORCE=false run_boot_subvols "$SHARED")" "WARN|0 updated, 1 skipped, 1 warnings"
check "boot plan, @ present, incremental: no snapshot call" "$(snap_calls)" "0"
# R5: the absence checks come before "exists, skip": no snapshot => WARN, not a skip.
check "boot, @ present, incremental, NO snapshot of its series: a WARN, not a skip (R5)" \
    "$(BOOT_PRESENT="/mnt/t/@ /mnt/t/@home" BOOT_FORCE=false run_boot_subvols "$WORK/none-big.txt")" "WARN|0 updated, 0 skipped, 2 warnings"
check "boot plan with no subdirs: WARN, said" \
    "$(BOOT_PLAN='@\troot-\t-\n' BOOT_FORCE=false run_boot_subvols "$SHARED")" "WARN|0 updated, 0 skipped, 1 warnings"
check "boot plan with no subdirs: the reason" "$(said_boot 'No source declares target_subdirs for @')" "1"
# 5. full, @ present: archive -> build staging -> delete live -> rename, in that order.
ONE='@\troot-\tnvme\n'
check "boot full, @ present: replaced" \
    "$(BOOT_PRESENT="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED")" "OK|1 updated, 0 skipped"
check "boot full, @ present: the exact sequence" \
    "$(boot_seq)" \
    "subvolume snapshot -r /mnt/t/@ /mnt/t/@.archive.TS;subvolume snapshot /mnt/t/nvme/root-.20261005T0100_1 /mnt/t/@.new;subvolume delete /mnt/t/@;mv -T -- /mnt/t/@.new /mnt/t/@;"
check "boot full, archive fails: the step FAILS" \
    "$(BOOT_FAIL_ON="snapshot -r" BOOT_PRESENT="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED")" "FAIL|0 updated, 1 failed"
check "boot full, archive fails: live never deleted, never renamed" "$(calls_of 'subvolume delete') $(calls_of '^mv')" "0 0"
check "boot full, staging build fails: FAIL, live never deleted" \
    "$(BOOT_FAIL_ON="@.new" BOOT_PRESENT="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(calls_of 'subvolume delete') $(calls_of '^mv')" \
    "FAIL|0 updated, 1 failed 0 0"
check "boot full, deleting live fails: FAIL, staging discarded, no rename" \
    "$(BOOT_FAIL_ON="delete /mnt/t/@" BOOT_PRESENT="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(calls_of 'subvolume delete /mnt/t/@.new') $(calls_of '^mv')" \
    "FAIL|0 updated, 1 failed 1 0"
check "boot full, the swap fails: FAIL, the archive is named" \
    "$(BOOT_MV_FAILS=1 BOOT_PRESENT="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(said_boot 'holds the previous contents')" \
    "FAIL|0 updated, 1 failed 1"
check "boot full, a stale @.new: deleted AFTER the archive, before the build" \
    "$(BOOT_PRESENT="/mnt/t/@ /mnt/t/@.new" BOOT_PLAN=$ONE run_boot_subvols "$SHARED" >/dev/null; boot_seq)" \
    "subvolume snapshot -r /mnt/t/@ /mnt/t/@.archive.TS;subvolume delete /mnt/t/@.new;subvolume snapshot /mnt/t/nvme/root-.20261005T0100_1 /mnt/t/@.new;subvolume delete /mnt/t/@;mv -T -- /mnt/t/@.new /mnt/t/@;"
check "boot full, the stale @.new cannot be removed: FAIL, live untouched" \
    "$(BOOT_FAIL_ON="delete /mnt/t/@.new" BOOT_PRESENT="/mnt/t/@ /mnt/t/@.new" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(calls_of 'subvolume delete /mnt/t/@$') $(calls_of '^mv')" \
    "FAIL|0 updated, 1 failed 0 0"
# 7. the listing cannot be read: FAIL for the target, nothing touched.
check "boot, the listing fails: the step FAILS, counted once" \
    "$(BOOT_LIST_RC=1 BOOT_PRESENT="/mnt/t/@" run_boot_subvols "$SHARED")" "FAIL|0 updated, 1 failed"
check "boot, the listing fails: says why, with the exit status" \
    "$(said_boot "[ERROR]   [das-backup-test] Could not list subvolumes: ERROR: can't access '/mnt/t': Input/output error (stub) (exit 1)")" "1"
check "boot, the listing fails: no snapshot, delete or mv" \
    "$(snap_calls) $(calls_of 'subvolume delete') $(calls_of '^mv')" "0 0 0"
# R7: a listing whose capture bash could not make is no answer, not an empty one.
check "boot, the listing capture yields no status line: FAIL, not a WARN for 'no snapshot'" \
    "$(BOOT_LIST_VANISH=1 BOOT_PRESENT="/mnt/t/@" run_boot_subvols "$SHARED")" "FAIL|0 updated, 1 failed"
check "boot, the listing capture yields no status line: said, and nothing touched" \
    "$(said_boot 'Could not list subvolumes: no answer') $(said_boot 'No btrbk snapshot named') $(snap_calls) $(calls_of 'subvolume delete') $(calls_of '^mv')" "1 0 0 0 0"
check "boot, an empty listing with status 0: the WARN, as before" \
    "$(: >"$WORK/empty-list.txt"; BOOT_FORCE=false run_boot_subvols "$WORK/empty-list.txt")" "WARN|0 updated, 0 skipped, 2 warnings"
# R5: an existence test that cannot say yes or no is a FAIL, nothing mutated.
check "boot, @ existence cannot be told: FAIL for it, no snapshot, no delete, no mv" \
    "$(BOOT_STAT_ERR="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(snap_calls) $(calls_of 'subvolume delete') $(calls_of '^mv')" \
    "FAIL|0 updated, 1 failed 0 0 0"
check "boot, @ existence cannot be told: said" "$(said_boot 'Cannot tell whether /mnt/t/@ exists')" "1"
check "boot full, @.new existence cannot be told: FAIL BEFORE the archive (nothing mutated)" \
    "$(BOOT_STAT_ERR="/mnt/t/@.new" BOOT_PRESENT="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(snap_calls)" \
    "FAIL|0 updated, 1 failed 0"

# The probe's three answers (bd DAS-Backup-Manager-jlsz). `mountpoint -q ... ||
# continue` read every failure of the probe as "not mounted": the target was
# skipped, counted as neither skipped nor failed, and the step recorded OK, 0
# updated, 0 skipped. Only "not mounted" leaves a target alone; "could not
# tell" fails the step, counted and said, and the other targets are still done.

check "boot subvolumes, the probe says mounted: the listing is read" \
    "$(BOOT_PROBES="/mnt/t=mounted" run_boot_subvols "$WORK/none-big.txt") $(boot_btrfs_calls)" \
    "WARN|0 updated, 0 skipped, 2 warnings filesystem label /mnt/t;subvolume list /mnt/t;"

check "boot subvolumes, the probe says not mounted: a quiet skip, as before" \
    "$(BOOT_PROBES="/mnt/t=notmounted" run_boot_subvols "$WORK/none-big.txt")" "OK|0 updated, 0 skipped"
check "boot subvolumes, not mounted: nothing said, btrfs never asked" \
    "$(said_boot '[ERROR]') $(boot_btrfs_calls)" "0 none"

check "boot subvolumes, no such path (an absent drive's mount point): a quiet skip too" \
    "$(BOOT_PROBES="/mnt/t=absent" run_boot_subvols "$WORK/none-big.txt")" "OK|0 updated, 0 skipped"
check "boot subvolumes, no such path: nothing said, btrfs never asked" \
    "$(said_boot '[ERROR]') $(boot_btrfs_calls)" "0 none"

check "boot subvolumes, the probe cannot tell: the step FAILS, counted" \
    "$(BOOT_PROBES="/mnt/t=error" run_boot_subvols "$WORK/none-big.txt")" "FAIL|0 updated, 1 failed"
check "boot subvolumes, cannot tell: said, with the target and the probe's own message" \
    "$(said_boot '[ERROR]   Could not tell whether /mnt/t is mounted — mountpoint: /mnt/t: Input/output error (exit 1); its boot subvolumes were NOT updated')" "1"
check "boot subvolumes, cannot tell: btrfs never asked — nothing was touched" "$(boot_btrfs_calls)" "none"

# A mirror the probe cannot tell about (independent review, M1). The step
# never updates a mirror's boot subvolumes — it carries another OS — so the
# wording above, "its boot subvolumes were NOT updated", misstates what
# happened to it. It still FAILS and is counted (a probe that errs on any
# target is worth a failed run, and the unmount gate fails on the same probe),
# and it is said as what it is: a mirror that could not be checked.
check "boot subvolumes, a mirror the probe cannot tell about: the step still FAILS, counted" \
    "$(BOOT_MIRRORS=/mnt/t BOOT_PROBES="/mnt/t=error" run_boot_subvols "$WORK/none-big.txt")" "FAIL|0 updated, 1 failed"
check "boot subvolumes, a mirror cannot tell: says it is a mirror that could not be checked, with the probe's own message" \
    "$(said_boot "[ERROR]   Could not tell whether /mnt/t, a mirror, is mounted — mountpoint: /mnt/t: Input/output error (exit 1); it could not be checked (its boot subvolumes are never updated here)")" "1"
check "boot subvolumes, a mirror cannot tell: nothing claims boot subvolumes were left un-updated" \
    "$(said_boot 'NOT updated')" "0"
check "boot subvolumes, a mirror cannot tell: btrfs never asked" "$(boot_btrfs_calls)" "none"
check "boot subvolumes, a mounted mirror: the quiet skip, as before" \
    "$(BOOT_MIRRORS=/mnt/t BOOT_PROBES="/mnt/t=mounted" run_boot_subvols "$WORK/none-big.txt")" "OK|0 updated, 1 skipped"
check "boot subvolumes, a mirror that is not mounted: a quiet skip, as before" \
    "$(BOOT_MIRRORS=/mnt/t BOOT_PROBES="/mnt/t=notmounted" run_boot_subvols "$WORK/none-big.txt")" "OK|0 updated, 0 skipped"
check "boot subvolumes, a primary and a mirror that cannot tell: each says its own, the step counts both" \
    "$(BOOT_MOUNTS="/mnt/t /mnt/u" BOOT_MIRRORS=/mnt/u BOOT_PROBES="/mnt/t=error /mnt/u=error" run_boot_subvols "$WORK/none-big.txt") $(said_boot ' /mnt/t is mounted — mountpoint: /mnt/t: Input/output error (exit 1); its boot subvolumes were NOT updated') $(said_boot ' /mnt/u, a mirror, is mounted — mountpoint: /mnt/u: Input/output error (exit 1); it could not be checked')" \
    "FAIL|0 updated, 2 failed 1 1"

# No mountpoint program at all (exit 127), the case the tracker names: the old
# code skipped the target and recorded OK.
check "boot subvolumes, no mountpoint program: the step FAILS, counted" \
    "$(BOOT_NO_MOUNTPOINT=1 run_boot_subvols "$WORK/none-big.txt")" "FAIL|0 updated, 1 failed"
check "boot subvolumes, no mountpoint program: said, with the shell's own words and the exit status" \
    "$(grep -c '\[ERROR\]   Could not tell whether /mnt/t is mounted — .*mountpoint: command not found (exit 127); its boot subvolumes were NOT updated' "$WORK/boot.out")" "1"
check "boot subvolumes, no mountpoint program: btrfs never asked" "$(boot_btrfs_calls)" "none"

# One target the probe cannot tell about fails the WHOLE step, in either order:
# nothing is asked of btrfs on any target, as the Rust step's (bd azvo).
check "boot subvolumes, first target cannot tell, second mounted: FAIL, the second is NOT done" \
    "$(BOOT_MOUNTS="/mnt/t /mnt/u" BOOT_PROBES="/mnt/t=error /mnt/u=mounted" run_boot_subvols "$WORK/none-big.txt") $(said_boot "No btrbk snapshot named 'root-'") $(boot_btrfs_calls)" \
    "FAIL|0 updated, 1 failed 0 none"
check "boot subvolumes, first target mounted, second cannot tell: FAIL, the first is NOT done" \
    "$(BOOT_MOUNTS="/mnt/t /mnt/u" BOOT_PROBES="/mnt/t=mounted /mnt/u=error" run_boot_subvols "$WORK/none-big.txt") $(said_boot "No btrbk snapshot named 'root-'") $(boot_btrfs_calls)" \
    "FAIL|0 updated, 1 failed 0 none"
check "boot subvolumes, cannot tell on one target, a replaceable @ on the other: nothing written" \
    "$(BOOT_MOUNTS="/mnt/t /mnt/u" BOOT_PROBES="/mnt/t=mounted /mnt/u=error" BOOT_PRESENT="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(snap_calls) $(calls_of 'subvolume delete') $(calls_of '^mv')" \
    "FAIL|0 updated, 1 failed 0 0 0"
check "boot subvolumes, one target cannot tell, one not mounted: FAIL, counted once" \
    "$(BOOT_MOUNTS="/mnt/t /mnt/u" BOOT_PROBES="/mnt/t=error /mnt/u=notmounted" run_boot_subvols "$WORK/none-big.txt")" "FAIL|0 updated, 1 failed"

# A probe that is itself broken (bd DAS-Backup-Manager-hhow). bash returns 0
# and an empty string for a command substitution it cannot make, so a probe
# read from its status read "mounted" exactly when it could not run. Its
# answer is a printed line now, and one that is empty or unrecognised is
# "could not tell": neither "mounted" nor "not mounted".
check "boot subvolumes, a probe that prints nothing: the step FAILS, counted" \
    "$(BOOT_PROBE_STUB=silent run_boot_subvols "$WORK/none-big.txt")" "FAIL|0 updated, 1 failed"
check "boot subvolumes, a probe that prints nothing: said, and btrfs never asked" \
    "$(said_boot '[ERROR]   Could not tell whether /mnt/t is mounted — the mount probe printed nothing (its capture failed?); its boot subvolumes were NOT updated') $(boot_btrfs_calls)" \
    "1 none"
check "boot subvolumes, a probe that prints something unrecognised: the step FAILS, counted" \
    "$(BOOT_PROBE_STUB=garbage run_boot_subvols "$WORK/none-big.txt")" "FAIL|0 updated, 1 failed"
check "boot subvolumes, a probe that prints something unrecognised: said, naming it, btrfs never asked" \
    "$(said_boot '[ERROR]   Could not tell whether /mnt/t is mounted — the mount probe printed something unrecognised: banana; its boot subvolumes were NOT updated') $(boot_btrfs_calls)" \
    "1 none"

# The same under real descriptor starvation, `ulimit -n` 3 to 10, with a
# mountpoint that needs no descriptor. At 3 nothing at all can be captured; the
# probe was read as "mounted" there and at 4 (the step went on to a target it
# had not looked at and recorded OK, 1 skipped), as it was read as "not
# mounted" before 4.11.3 (OK, 0 skipped). Whatever the limit now: a probe that
# can only say "not mounted" is never read as "mounted", so the step is either
# a quiet skip or a FAIL, and one that errs is a FAIL.
boot_at_limit() { # boot_at_limit <limit>: the step's "<result>|<detail>" with that many descriptors
    BOOT_LIMIT="$1" run_boot_subvols "$WORK/none-big.txt" >/dev/null
    sed -n 's/^RESULT //p' "$WORK/boot.out"
}
for limit in 3 4 5 6 7 8 9 10; do
    got_not="$(BOOT_PROBES="/mnt/t=notmounted" boot_at_limit "$limit")"
    got_err="$(BOOT_PROBES="/mnt/t=error" boot_at_limit "$limit")"
    verdict=ok
    case "$got_not" in
    "OK|0 updated, 0 skipped" | "FAIL|0 updated, 1 failed") ;;
    *) verdict="a probe saying not mounted gave '$got_not'" ;;
    esac
    [[ "$got_err" == "FAIL|0 updated, 1 failed" ]] || verdict="a probe that errs gave '$got_err'"
    check "boot subvolumes at descriptor limit $limit: no probe is read as the opposite of what it says" "$verdict" "ok"
done
check "boot subvolumes at descriptor limit 3: nothing can be captured, so the step FAILS whatever the probe says" \
    "$(BOOT_PROBES="/mnt/t=notmounted" boot_at_limit 3) / $(BOOT_PROBES="/mnt/t=error" boot_at_limit 3)" \
    "FAIL|0 updated, 1 failed / FAIL|0 updated, 1 failed"

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
# rows, whole outputs with -c, -c's execution status for all 256 bytes, the
# never-run line, the checksum warnings. The SCSI lines rest on
# scsiprint.cpp's format strings alone. Each goes through the real
# check_smart_tests and the helper defined after it, with smartctl a stub per
# drive (taking -b exit and -c as the binary does), no smartctl on PATH, and
# device paths that are not devices.
extract_upto das-partition-drives.sh check_smart_tests show_plan 'out="$(smartctl '
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
# The drive's CURRENT self-test execution status, as -c prints it on ATA
# (ataprint.cpp PrintSmartSelfExecStatus): the byte, then its meaning by the
# high nibble; 15 is a test in progress, the low nibble its tens of percent
# left (zet1). Byte for byte as the smartctl 7.5 binary prints all 256 values.
ata_exec_status() { # ata_exec_status <status byte>
    local t=$'\n\t\t\t\t\t'
    printf 'Self-test execution status:      (%4d)\t' "$1"
    case $(($1 >> 4)) in
        0) printf '%s\n' "The previous self-test routine completed${t}without error or no self-test has ever ${t}been run." ;;
        1) printf '%s\n' "The self-test routine was aborted by${t}the host." ;;
        2) printf '%s\n' "The self-test routine was interrupted${t}by the host with a hard or soft reset." ;;
        3) printf '%s\n' "A fatal error or unknown test error${t}occurred while the device was executing${t}its self-test routine and the device ${t}was unable to complete the self-test ${t}routine." ;;
        4) printf '%s\n' "The previous self-test completed having${t}a test element that failed and the test${t}element that failed is not known." ;;
        5) printf '%s\n' "The previous self-test completed having${t}the electrical element of the test${t}failed." ;;
        6) printf '%s\n' "The previous self-test completed having${t}the servo (and/or seek) element of the ${t}test failed." ;;
        7) printf '%s\n' "The previous self-test completed having${t}the read element of the test failed." ;;
        8) printf '%s\n' "The previous self-test completed having${t}a test element that failed and the${t}device is suspected of having handling${t}damage." ;;
        15) printf '%s\n' "Self-test routine in progress...${t}$(($1 & 15))0% of test remaining." ;;
        *) printf '%s\n' "Reserved." ;;
    esac
}
# All that -c adds for the drive the replays used: "General SMART Values",
# its execution status among them, and the blank line after it.
ata_general() { # ata_general <status byte>
    local t=$'\t\t\t\t\t'
    printf '%s\n' 'General SMART Values:' \
        $'Offline data collection status:  (0x00)\tOffline data collection activity' \
        "${t}was never started." "${t}Auto Offline Data Collection: Disabled."
    ata_exec_status "$1"
    printf '%s\n' 'Total time to complete Offline ' $'data collection: \t\t(    0) seconds.' \
        'Offline data collection' $'capabilities: \t\t\t (0x5b) SMART execute Offline immediate.' \
        "${t}Auto Offline data collection on/off support." "${t}Suspend Offline collection upon new" \
        "${t}command." "${t}Offline surface scan supported." "${t}Self-test supported." \
        "${t}No Conveyance Self-test supported." "${t}Selective Self-test supported." \
        $'SMART capabilities:            (0x0003)\tSaves SMART data before entering' \
        "${t}power-saving mode." "${t}Supports SMART auto save timer." \
        $'Error logging capability:        (0x01)\tError logging supported.' \
        "${t}No General Purpose Logging support." \
        'Short self-test routine ' $'recommended polling time: \t (   1) minutes.' \
        'Extended self-test routine' $'recommended polling time: \t ( 255) minutes.' ''
}
ata_log() { # ata_log <rows> [<current status byte>, 0]: what `smartctl -c -l selftest` prints for an ATA drive
    printf '%s\n' "$BANNER" '=== START OF READ SMART DATA SECTION ==='
    ata_general "${2:-0}"
    printf '%s\n' 'SMART Self-test log structure revision number 1' \
        'Num  Test_Description    Status                  Remaining  LifeTime(hours)  LBA_of_first_error' "$1"
}
# What smartctl prints by default (-b warn) when a structure it reads fails
# its checksum: its warning where it reads that structure (IDENTIFY and SMART
# data before the section line, the self-test log after -c's section), then
# the log as ever, at exit 0. Under -b exit that warning is its last line, at
# exit 4: the stub below does so.
ata_log_bad_checksum() { # ata_log_bad_checksum <structure> <rows>
    local log section='=== START OF READ SMART DATA SECTION ===' warning="Warning! $1 error: invalid SMART checksum."
    local revision='SMART Self-test log structure revision number 1'
    log="$(ata_log "$2")"
    if [[ $1 == "SMART Self-Test Log Structure" ]]; then
        printf '%s\n' "${log/"$revision"/"$warning"$'\n'"$revision"}"
    else
        printf '%s\n' "${log/"$section"/"$warning"$'\n'"$section"}"
    fi
}
# SCSI: -c prints nothing (smartctl.cpp sets no SCSI option for it). While a
# test runs, -l selftest itself prints its progress before the log
# (scsiprint.cpp, from REQUEST SENSE), and nothing otherwise.
scsi_log() { # scsi_log <rows> [<percent of a running test left>]: what it prints for a SCSI drive
    printf '%s\n' "$BANNER" '=== START OF READ SMART DATA SECTION ==='
    if [[ -n ${2:-} ]]; then printf 'Self-test execution status:\t\t%d%% of test remaining\n' "$2"; fi
    printf '%s\n' 'SMART Self-test log' \
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
        local out="${SMART_OUT[${!#}]}" rc="${SMART_RC[${!#}]}" head tail
        # Each fixture is what `-c -l selftest` prints. Without -c smartctl
        # prints no "General SMART Values" section: the section and the blank
        # line after it go (measured on the 7.5 binary, replayed, both ways).
        if [[ " $* " != *" -c "* && $out == *"General SMART Values:"* ]]; then
            head="${out%%General SMART Values:*}" tail="${out#*General SMART Values:}"
            out="$head${tail#*$'\n\n'}"
        fi
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
# row "# 1" it is: bit 7 for the failures it counts (0x3-0x8), else 0. Each
# drive's current status (-c) is the one a drive that logs at once shows
# beside it; a test in progress is then shown with its percentage left.
n=0
while IFS='|' read -r status code live want_rc verdict shown advice; do
    n=$((n + 1))
    rem=9 lba=-
    [[ $status == "Completed without error" ]] && rem=0
    [[ $status == "Completed: read failure" ]] && lba=123456
    fixture "ata-$n" "$(ata_log "$(ata_row 1 'Extended offline' "$status" "$rem" "$lba")" "$live")"
    check "ATA '$status', current status $live, smartctl exit $code: $verdict, advice $advice" \
        "$(gate "S1:ata-$n:$code")|$(said S1)|$(advised S1)" "$want_rc|$verdict — $shown|${ADVICE[$advice]}"
done <<'EOF'
Completed without error|0|0|0|PASSED|Completed without error|none
Aborted by host|0|25|1|NOT PASSED|Aborted by host|unfinished
Interrupted (host reset)|0|41|1|NOT PASSED|Interrupted (host reset)|unfinished
Fatal or unknown error|128|57|1|NOT PASSED|Fatal or unknown error|failed
Completed: unknown failure|128|73|1|NOT PASSED|Completed: unknown failure|failed
Completed: electrical failure|128|89|1|NOT PASSED|Completed: electrical failure|failed
Completed: servo/seek failure|128|105|1|NOT PASSED|Completed: servo/seek failure|failed
Completed: read failure|128|121|1|NOT PASSED|Completed: read failure|failed
Completed: handling damage??|128|137|1|NOT PASSED|Completed: handling damage??|failed
Unknown status (0x9)|0|153|1|NOT PASSED|Unknown status (0x9)|unrecognised
Self-test routine in progress|0|249|1|STILL RUNNING|self-test in progress, 90% remaining|running
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
fixture ata-none "$(printf '%s\n' "$BANNER" '=== START OF READ SMART DATA SECTION ==='
    ata_general 0
    printf '%s\n' 'SMART Self-test log structure revision number 1' \
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
check "the gate asks smartctl with -b exit" "$(grep -c '^-b exit -c -l selftest ' "$WORK/smartctl.calls")" "1"
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

# zet1: a drive that writes its log entry only when a test ENDS shows the
# previous test, passed, as row "# 1" while a new one runs; 2.3.2 read only
# the log, and passed it. The gate now reads -c's current status as well.
# These are the smartctl 7.5 binary's own output for such drives, replayed
# (its "-" device; no drive touched): a passed log under status 249 (90%
# left), 245, 241, 240 (0% left, still running), and 250, which smartctl
# prints as "100%" as it finds it.
for live in 249:90 245:50 241:10 240:0 250:100; do
    b=${live%%:*} pct=${live#*:}
    fixture "zet1-running-$b" "$(ata_log "$ATA_PASSED" "$b")"
    check "zet1: log passed, a test running now (current status $b, $pct% left): STILL RUNNING" \
        "$(gate "S1:zet1-running-$b:0")|$(said S1)|$(advised S1)" \
        "1|STILL RUNNING — self-test in progress, $pct% remaining; the log's newest entry is an earlier test: Completed without error|${ADVICE[running]}"
done
# A drive that logs a test as it starts agrees with itself: no note.
fixture zet1-running-logged "$(ata_log "$(ata_row 1 'Extended offline' 'Self-test routine in progress' 9 -)" 249)"
check "zet1: the log says running too: STILL RUNNING, with the percentage" \
    "$(gate "S1:zet1-running-logged:0")|$(said S1)" "1|STILL RUNNING — self-test in progress, 90% remaining"
# A test running now outranks an older result of any kind: it will be the
# newest once it ends.
fixture zet1-running-over-failed "$(ata_log "$(ata_row 1 'Extended offline' 'Completed: read failure' 9 123456)" 249)"
check "zet1: a failed row, a test running now: STILL RUNNING" \
    "$(gate "S1:zet1-running-over-failed:128")|$(said S1)|$(advised S1)" \
    "1|STILL RUNNING — self-test in progress, 90% remaining; the log's newest entry is an earlier test: Completed: read failure|${ADVICE[running]}"
# The current status says the last test did not pass, under a passed row: it
# ended without the log showing it, so the newest test is not the logged one.
while IFS='|' read -r b what key; do
    fixture "zet1-live-$b" "$(ata_log "$ATA_PASSED" "$b")"
    check "zet1: log passed, current status $b: NOT PASSED, advice $key" \
        "$(gate "S1:zet1-live-$b:0")|$(said S1)|$(advised S1)" \
        "1|NOT PASSED — the last self-test $what (smartctl -c, status $b); the log's newest entry is an earlier test: Completed without error|${ADVICE[$key]}"
done <<'EOF'
25|was aborted by the host|unfinished
41|was interrupted by the host with a reset|unfinished
57|could not complete due to a fatal or unknown error|failed
73|completed with error (unknown test element)|failed
89|completed with error (electrical test element)|failed
105|completed with error (servo/seek test element)|failed
121|completed with error (read test element)|failed
137|completed with error (handling damage?)|failed
153|has a status no standard defines|unrecognised
233|has a status no standard defines|unrecognised
EOF
# Status 0 (done, or never run) adds nothing and removes nothing: its low
# nibble does not count, and the log still decides.
fixture zet1-live-9 "$(ata_log "$ATA_PASSED" 9)"
check "zet1: current status 9 (high nibble 0) under a passed row: PASSED" \
    "$(gate "S1:zet1-live-9:0")|$(said S1)" "0|PASSED — Completed without error"
check "zet1: current status 0 under a failed row: NOT PASSED, the row decides" \
    "$(gate "S1:ata-read-failure:128")|$(said S1)" "1|NOT PASSED — Completed: read failure"
# No current status at all: smartctl could not read the SMART data, so -c had
# nothing to print, yet the log came (the binary's own output, exit 4).
fixture zet1-no-live "$(printf '%s\n' "$BANNER" 'Read SMART Data failed: Input/output error' '' \
    '=== START OF READ SMART DATA SECTION ===' 'SMART Self-test log structure revision number 1' \
    'Num  Test_Description    Status                  Remaining  LifeTime(hours)  LBA_of_first_error' "$ATA_PASSED")"
check "zet1: no current status (SMART data unreadable, exit 4): NOT PASSED, no result" \
    "$(gate "S1:zet1-no-live:4")|$(said S1)|$(advised S1)" \
    "1|NOT PASSED — no self-test result: smartctl printed no current execution status (exit 4)|${ADVICE[no-result]}"
# SCSI: no -c section; -l selftest's own progress line while a test runs
# (scsiprint.cpp's format; the replay device is ATA only).
fixture zet1-scsi-running "$(scsi_log "$SCSI_PASSED" 90)"
check "zet1: SCSI log passed, a test running now: STILL RUNNING" \
    "$(gate "S1:zet1-scsi-running:0")|$(said S1)|$(advised S1)" \
    "1|STILL RUNNING — self-test in progress, 90% remaining; the log's newest entry is an earlier test: Completed|${ADVICE[running]}"
fixture zet1-scsi-running-logged "$(scsi_log "$(scsi_row 1 'Background long ' 'Self test in progress ...' - NOW - -)" 37)"
check "zet1: SCSI, the log says running too: STILL RUNNING, with the percentage" \
    "$(gate "S1:zet1-scsi-running-logged:0")|$(said S1)" "1|STILL RUNNING — self-test in progress, 37% remaining"
check "the gate asks smartctl for -c beside the log, with -b exit" \
    "$(gate S1:ata-passed:0 >/dev/null; grep -cxF -e "-b exit -c -l selftest $WORK/not-a-device-S1" "$WORK/smartctl.calls")" "1"

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
check "three drives: smartctl asked once each, for the self-test log and -c, with -b exit" \
    "$(grep -c '^-b exit -c -l selftest .*/not-a-device-S[123]$' "$WORK/smartctl.calls")" "3"

# Output over 64 KiB, its row on line 1 or after it all. The filler lines
# start "# 1 " and one space: no row of smartctl's starts so.
filler_lines() { # filler_lines: 1500 lines, none of them a row
    local i
    for ((i = 0; i < 1500; i++)); do printf '# 1 filler line %06d of the self-test log, nothing to see\n' "$i"; done
}
# An ATA drive's current status (-c) goes at the other end from its row, so
# each is found across the 64 KiB too; SCSI prints none here.
for kind in ata-passed:0 ata-running:0 ata-read-failure:0 zet1-running-249:249 scsi-running:-; do
    name=${kind%%:*} live=${kind#*:} status_lines=""
    row="$(grep -m1 '^# 1  ' "$WORK/st-$name.txt")"
    if [[ $live != - ]]; then status_lines="$(ata_exec_status "$live")"; fi
    { echo "$row" && filler_lines && if [[ -n $status_lines ]]; then echo "$status_lines"; fi; } >"$WORK/st-big-first-$name.txt"
    big_size_ok "$WORK/st-big-first-$name.txt"
    { if [[ -n $status_lines ]]; then echo "$status_lines"; fi && filler_lines && echo "$row"; } >"$WORK/st-big-last-$name.txt"
    big_size_ok "$WORK/st-big-last-$name.txt"
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
    check "row on the $at line over 64 KiB, its log passed, a test running now: STILL RUNNING" \
        "$(gate "S1:big-$at-zet1-running-249:0")|$(said S1)" \
        "1|STILL RUNNING — self-test in progress, 90% remaining; the log's newest entry is an earlier test: Completed without error"
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
# zet1, end to end: a drive whose log says passed while it tests now.
check "main --run, a drive testing now under a passed log: exits 1 before YES-DESTROY" \
    "$(run_main --run S1:ata-passed:0 S2:zet1-running-249:0)" "1|"
check "main --run over it: named by serial, told to wait" \
    "$(grep -cF "  S2 (label-S2): STILL RUNNING — self-test in progress, 90% remaining" "$WORK/main.out")|$(grep -cF "      → ${ADVICE[running]}" "$WORK/main.out")" "1|1"
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
# The boot step's matcher is latest_boot_snapshot now: one word split and
# `[[ =~ ]]`, which need no descriptor either.
no_fd_boot_walk() {
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/latest_boot_snapshot.sh"
        listing="$(cat "$WORK/series-at-end.txt")"
        ulimit -n 3
        latest_boot_snapshot "$listing" root- nvme
        printf '%s' "$LATEST_BOOT_SNAPSHOT"
    ) 2>/dev/null
}
check "no fd to spare: the boot snapshot walk still finds a series at the end of 64 KiB" \
    "$(no_fd_boot_walk)" "nvme/root-.20261010T0100"
check "no fd to spare: the SMART check still sees PASSED" \
    "$(with_no_fd_to_spare "$(condition backup-verify.sh '^[[:space:]]*if .*PASSED')" \
        health "$PASSED_LINE")" "match"

# ---------------------------------------------------------------------------
echo "== the boot snapshot match's digits are ASCII digits, whatever the locale"
# ---------------------------------------------------------------------------
# grep's [0-9] matched ASCII digits only. Under en_US.UTF-8 — the host's
# locale — bash's regex [0-9] also matches Arabic-Indic and fullwidth digits,
# so a name like root.٢٠٢٦١٠٠٤T٠٣٠٠ read as btrbk-shaped (round 5, N5;
# measured). [[:digit:]] is ASCII only: the old meaning exactly. Such a name
# is no btrbk snapshot, so no series matches it: a WARN for each subvolume of
# the plan (latest_boot_snapshot compares under its own LC_ALL=C).
NONASCII_ARABIC='ID 301 gen 9 top level 5 path nvme/root-.٢٠٢٦١٠٠٤T٠٣٠٠'
NONASCII_FULLWIDTH='ID 302 gen 9 top level 5 path nvme/root-.２０２６１００４T０３００'
# Only where the locale shows the difference: bash's own [0-9] must match a
# non-ASCII digit there, or these checks could not fail.
not_run=""
probe_digit='٢'
if (export LC_ALL=en_US.UTF-8; [[ $probe_digit =~ [0-9] ]]) 2>/dev/null; then # locale-range-ok: the probe
    for name in NONASCII_ARABIC NONASCII_FULLWIDTH; do
        printf '%s\n' "${!name}" >"$WORK/$name.txt"
        check "en_US.UTF-8, the only planned-name snapshot has non-ASCII digits ($name): no match, a WARN" \
            "$(BOOT_LOCALE=en_US.UTF-8 run_boot_subvols "$WORK/$name.txt")" "WARN|0 updated, 0 skipped, 2 warnings"
    done
    check "en_US.UTF-8, an ASCII series of another name: still the WARN" \
        "$(BOOT_LOCALE=en_US.UTF-8 run_boot_subvols "$WORK/drift-small.txt")" "WARN|0 updated, 0 skipped, 2 warnings"
    check "en_US.UTF-8, an ASCII timestamp is still matched" \
        "$(BOOT_LOCALE=en_US.UTF-8 BOOT_FORCE=false run_boot_subvols "$WORK/series-at-end.txt")" "OK|2 updated, 0 skipped"
else
    not_run="the en_US.UTF-8 cases: bash's [0-9] matches no non-ASCII digit here (locale missing?)"
    echo "NOT RUN: $not_run"
fi

# ---------------------------------------------------------------------------
echo "== backup-verify.sh: report_sector_attr, a count is ASCII digits (bd 1bsx)"
# ---------------------------------------------------------------------------
# The same locale effect as above, at the SMART check: under en_US.UTF-8 bash's
# regex [0-9] also matches digits of other scripts and superscripts, so a
# reallocated or pending sector "count" written with one was printed in yellow
# as a number and returned success. It is a reading no one can use: UNKNOWN,
# and a nonzero return, like any other value that is not a number.
extract backup-verify.sh report_sector_attr 'unparsable value'

run_sector_attr() { # run_sector_attr <value> [locale]: "<status>|<what it printed>"
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/report_sector_attr.sh"
        RED='<red>' GREEN='<green>' YELLOW='<yellow>' NC='<end>'
        if [[ -n "${2:-}" ]]; then
            export LC_ALL="$2"
        fi
        rc=0
        out="$(report_sector_attr "Pending Sectors" "$1")" || rc=$?
        printf '%s|%s\n' "$rc" "$out"
    )
}
check "sector attribute 0: green" "$(run_sector_attr 0)" "0|  Pending Sectors: <green>0<end>"
check "sector attribute 7: a count, in yellow" "$(run_sector_attr 7)" "0|  Pending Sectors: <yellow>7<end>"
check "sector attribute, smartctl failed: UNKNOWN, nonzero" "$(run_sector_attr SMARTCTL_FAILED)" \
    "1|  Pending Sectors: <red>UNKNOWN (smartctl could not read this device)<end>"
check "sector attribute, not reported: UNKNOWN, nonzero" "$(run_sector_attr NOT_PRESENT)" \
    "1|  Pending Sectors: <red>UNKNOWN (attribute not reported by this device)<end>"
check "sector attribute 12x: unparsable, nonzero" "$(run_sector_attr 12x)" \
    "1|  Pending Sectors: <red>UNKNOWN (unparsable value: 12x)<end>"

ARABIC_THREE=$'\xd9\xa3'
SUPERSCRIPT_TWO=$'\xc2\xb2'
if (export LC_ALL=en_US.UTF-8; [[ $ARABIC_THREE =~ [0-9] ]]) 2>/dev/null; then # locale-range-ok: the probe
    for digit in "$ARABIC_THREE" "$SUPERSCRIPT_TWO" "1$ARABIC_THREE"; do
        check "en_US.UTF-8, sector attribute '$digit': unparsable, not a count" \
            "$(run_sector_attr "$digit" en_US.UTF-8)" \
            "1|  Pending Sectors: <red>UNKNOWN (unparsable value: $digit)<end>"
    done
    check "en_US.UTF-8, sector attribute 7: still a count" \
        "$(run_sector_attr 7 en_US.UTF-8)" "0|  Pending Sectors: <yellow>7<end>"
else
    not_run+="${not_run:+; }the en_US.UTF-8 sector-attribute cases: bash's [0-9] matches no non-ASCII digit here (locale missing?)"
    echo "NOT RUN: the en_US.UTF-8 sector-attribute cases"
fi

# ---------------------------------------------------------------------------
echo "== no bracket range in a regex match anywhere in the shell sources (bd 1bsx)"
# ---------------------------------------------------------------------------
# bash's regex follows the locale's collation. Under en_US.UTF-8 a range such
# as [0-9] or [1-9] matches about 1,000 characters beyond ASCII digits and
# [A-Za-z] about 2,200 beyond ASCII letters (all 1.1 million non-ASCII code
# points were tried), where [[:digit:]] matches none. [[:alpha:]] is no
# substitute for letters — it is every letter the locale has — so they are
# listed. Five matches that read a guard, a pid, a bay number, a SMART value
# and an archive name were found with the range in them; none may return.
#
# What the lint reads, per file, in two passes. The first collects every name
# a regex match reads its pattern from, UNQUOTED: `=~ $x` and `=~ ${x}` (a
# quoted one is matched literally and cannot widen). The second flags a line
# when it holds a range in (1) the text after a =~, (2) the text after the = of
# an assignment to a *_RE or re variable, whatever the file does with it, or
# (3) the text after the = of an assignment (plain, local, readonly, declare,
# export, or +=) to any name the first pass collected, wherever the use is in
# the file. (3) is what keeps the deletion guard's own patterns in view: they
# moved off the =~ line into lower-case locals (path_re, name_re), where a
# line-at-a-time reading of =~ and upper-case names saw neither the use nor the
# assignment. An array literal (name=( ... )) holds subscripts, not a pattern.
# Comment lines are skipped. A deliberate use carries "locale-range-ok" on its
# line: the probes that decide whether a locale shows the effect at all.
# Not followed: a pattern built in one file and matched in another, and the
# patterns of grep and sed, which are not bash's regex.
range_lint() { # range_lint <files>: file:line: text, for each offender
    local f
    for f in "$@"; do
        LC_ALL=C awk '
            function ranged(s) { return s ~ /\[[^]]*[0-9A-Za-z]-[0-9A-Za-z][^]]*\]/ }
            FNR == NR {
                if ($0 ~ /^[ \t]*#/) next
                s = $0
                while (match(s, /=~[ \t]*\$\{?[A-Za-z_][A-Za-z0-9_]*/)) { # locale-range-ok: awk, not a bash match
                    name = substr(s, RSTART, RLENGTH)
                    sub(/^=~[ \t]*\$\{?/, "", name)
                    used[name] = 1
                    s = substr(s, RSTART + RLENGTH)
                }
                next
            }
            /^[ \t]*#/ { next }
            /locale-range-ok/ { next }
            {
                bad = 0
                if (match($0, /=~|_RE=|[ \t]re=/) && ranged(substr($0, RSTART))) bad = 1
                for (n in used) {
                    if (!bad && match($0, "(^|[ \t;])" n "\\+?=")) {
                        rest = substr($0, RSTART + RLENGTH)
                        if (rest !~ /^\(/ && ranged(rest)) bad = 1
                    }
                }
                if (bad) print FILENAME ":" FNR ": " $0
            }' "$f" "$f"
    done
}
# The lint must be able to say no. A planted range in a regex match is found,
# and so is one in a variable the file matches against, in the shapes the
# tree uses (a lower-case local, a ${braced} use, one of two assignments on a
# line, an append, an assignment AFTER its use); the forms that look alike and
# are not — [[:digit:]], an array subscript before the =~, a comment line, a
# marked probe, an array literal, a range in a variable no =~ reads, and one
# read only quoted — are not.
{
    printf '%s\n' '[[ $x =~ ^[0-9]+$ ]]'    # locale-range-ok: lint fixture
    printf '%s\n' '[[ $x =~ ^[A-Za-z]+$ ]]' # locale-range-ok: lint fixture
    printf '%s\n' 'local re="^[a-f0-9]+$"'  # locale-range-ok: lint fixture
    printf '%s\n' 'local path_re="^[A-Z]+$"' # locale-range-ok: lint fixture
    printf '%s\n' '[[ $p =~ $path_re ]]'
    printf '%s\n' 'local keep="^[a-z]+$" lim="^[0-9]+$"' # locale-range-ok: lint fixture
    printf '%s\n' '[[ $y =~ ${lim} ]]'
    printf '%s\n' "pat+='[0-9]'" # locale-range-ok: lint fixture
    printf '%s\n' '[[ $z =~ $pat ]]'
    printf '%s\n' '[[ $q =~ $late ]]'
    printf '%s\n' "late='^[0-9]+\$'" # locale-range-ok: lint fixture
} >"$WORK/lint-bad.sh"
{
    printf '%s\n' '[[ $x =~ ^[[:digit:]]+$ ]]'
    printf '%s\n' 'declare -A m=([hdd-media]=1); [[ $y =~ $m ]]'
    printf '%s\n' '# [[ $x =~ ^[0-9]+$ ]] in a comment' # locale-range-ok: lint fixture
    printf '%s\n' '[[ $x =~ [0-9] ]] # locale-range-ok: a probe'
    printf '%s\n' 'local path_re="^[${letters}[:digit:]_@.+/-]+\$"'
    printf '%s\n' '[[ $p =~ $path_re ]]'
    printf '%s\n' 'local globby="[a-z]*"' # a glob, and no =~ reads it
    printf '%s\n' '[[ $g == $globby ]]'
    printf '%s\n' 'local lit="[a-z]"' # read quoted below: matched literally
    printf '%s\n' '[[ $l =~ "$lit" ]]'
} >"$WORK/lint-ok.sh"
check "the range lint finds a planted digit range, letter range and hex range in a variable, and four more in variables a =~ reads" "$(range_lint "$WORK/lint-bad.sh" | wc -l)" "7"
check "the range lint passes [[:digit:]], a subscript, a comment, a marked probe, an array literal, a glob and a literal match" "$(range_lint "$WORK/lint-ok.sh" | wc -l)" "0"
shell_sources=("$ROOT"/scripts/*.sh "$ROOT"/.github/scripts/*.sh "$ROOT"/packaging/appimage/*.sh "$ROOT"/tests/*.sh)
[[ -e "${shell_sources[0]}" && -e "${shell_sources[${#shell_sources[@]} - 1]}" ]] || harness_broken "a source glob matched nothing"
left="$(range_lint "${shell_sources[@]}" | sed "s|^$ROOT/||")"
check "no bracket range in a regex match under scripts/, .github/scripts/, packaging/, tests/" "${left:-none}" "none"

# ---------------------------------------------------------------------------
echo "== no producer | grep -q left in scripts/"
# ---------------------------------------------------------------------------
# Every `if`/`elif` that pipes into grep -q under these scripts' pipefail is
# the shape above. None may come back; a comment may still quote one.
left="$(awk '!/^[[:space:]]*#/ && /\|[[:space:]]*grep([[:space:]]+-[a-zA-Z]+)*[[:space:]]+-[a-zA-Z]*q/ { print FILENAME ":" FNR ": " $0 }' "$ROOT"/scripts/*.sh)"
check "no 'producer | grep -q' in scripts/" "${left:-none}" "none"

echo "--- tens/azvo: archive destination and symmetry"
# bd tens: `btrfs subvolume snapshot -r <live> <existing directory>` nests the
# snapshot inside it and succeeds, so the archive destination is checked
# (fail-closed, like the live and staging paths) before anything is written.
no_writes() { echo "$(snap_calls) $(calls_of 'subvolume delete') $(calls_of '^mv')"; }
check "boot full, the archive destination exists: FAIL, nothing written" \
    "$(BOOT_PRESENT="/mnt/t/@ /mnt/t/@.archive.*" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(no_writes)" \
    "FAIL|0 updated, 1 failed 0 0 0"
check "boot full, the archive destination exists: said, naming it, live left untouched" \
    "$(said_boot 'archive destination /mnt/t/@.archive.') $(said_boot 'already exists — leaving @ untouched')" "1 1"
check "boot full, the archive destination is a symlink: FAIL, nothing written" \
    "$(BOOT_PRESENT="/mnt/t/@" BOOT_SYMLINK="/mnt/t/@.archive.*" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(no_writes)" \
    "FAIL|0 updated, 1 failed 0 0 0"
check "boot full, the archive destination cannot be statted: FAIL BEFORE any write" \
    "$(BOOT_PRESENT="/mnt/t/@" BOOT_STAT_ERR="/mnt/t/@.archive.*" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(no_writes)" \
    "FAIL|0 updated, 1 failed 0 0 0"
check "boot full, the archive destination cannot be statted: said" \
    "$(said_boot 'Cannot tell whether /mnt/t/@.archive.')" "1"
check "boot full, the archive destination absent (the control): the archive is made" \
    "$(BOOT_PRESENT="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(snap_calls)" "OK|1 updated, 0 skipped 2"
check "boot incremental, @ present, an archive path in the way: skipped, never reached" \
    "$(BOOT_FORCE=false BOOT_PRESENT="/mnt/t/@ /mnt/t/@.archive.*" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(no_writes)" \
    "OK|0 updated, 1 skipped 0 0 0"

# bd azvo (i): a symlink at the live path is no subvolume, in either mode.
check "boot full, a symlink at the live path: FAIL, nothing written" \
    "$(BOOT_SYMLINK="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(no_writes)" \
    "FAIL|0 updated, 1 failed 0 0 0"
check "boot full, a symlink at the live path: said" \
    "$(said_boot '/mnt/t/@ is a symbolic link, not a subvolume — leaving @ untouched')" "1"
check "boot incremental, a symlink at the live path: FAIL, not a skip and not a create" \
    "$(BOOT_FORCE=false BOOT_SYMLINK="/mnt/t/@" BOOT_PLAN=$ONE run_boot_subvols "$SHARED") $(no_writes)" \
    "FAIL|0 updated, 1 failed 0 0 0"
check "boot, a symlink at the live path and a plain directory for the next subvolume: only the first fails" \
    "$(BOOT_FORCE=false BOOT_SYMLINK="/mnt/t/@" BOOT_PRESENT="/mnt/t/@home" run_boot_subvols "$SHARED") $(said_boot '@home exists, skipping')" \
    "FAIL|0 updated, 1 failed 1"

# bd azvo (ii): no targets configured is a WARN, with nothing asked of anything.
check "boot, no targets configured: WARN, not OK" \
    "$(BOOT_MOUNTS=" " run_boot_subvols "$SHARED")" "WARN|0 updated, 0 skipped, 1 warnings"
check "boot, no targets configured: said, and neither btrfs nor the plan asked" \
    "$(said_boot 'No backup targets configured') $(boot_btrfs_calls) $(wc -c <"$WORK/btrdasd.calls")" "1 none 0"

# bd azvo note 2: an unmounted mirror is said (the Rust step's Info line), and
# still not counted.
check "boot, a mirror that is not mounted: said, counts unchanged" \
    "$(BOOT_MIRRORS=/mnt/t BOOT_PROBES="/mnt/t=notmounted" run_boot_subvols "$WORK/none-big.txt") $(said_boot '[INFO]   [/mnt/t] Not mounted — mirror target left alone (independent OS)')" \
    "OK|0 updated, 0 skipped 1"
check "boot, a primary that is not mounted: nothing said about a mirror" \
    "$(BOOT_PROBES="/mnt/t=notmounted" run_boot_subvols "$WORK/none-big.txt") $(said_boot 'mirror target left alone')" \
    "OK|0 updated, 0 skipped 0"
check "boot, a primary that is not mounted: said with its configured label, counts unchanged" \
    "$(BOOT_PROBES="/mnt/t=notmounted" run_boot_subvols "$WORK/none-big.txt") $(said_boot "[INFO]   [/mnt/t] Not mounted — boot subvolumes not updated on 'label-t'")" \
    "OK|0 updated, 0 skipped 1"

# bd azvo (iv): the listing path is the last whitespace-delimited field,
# trailing whitespace and CR dropped; the same rows the Rust suite asserts.
check "shared listing: a trailing space on the line" "$(latest_of nvme sp-)" "nvme/sp-.20261001T0100"
check "shared listing: a trailing tab on the line" "$(latest_of nvme tb-)" "nvme/tb-.20261001T0100"
check "shared listing: a trailing CR on the line (CRLF)" "$(latest_of nvme cr-)" "nvme/cr-.20261001T0100"

# ---------------------------------------------------------------------------
echo "--- 5bwi/nqbb: linear walk and boot coverage"
# ---------------------------------------------------------------------------
# 5bwi: the listing walk is one word split, so its cost grows with the listing
# and not with its square (0.27 s at 1,500 lines, 27 s at 15,000 before). The
# checks: the answer at the three sizes the cost was measured at, the match
# last (the whole listing must be read), and a wall-clock bound generous for a
# loaded host yet far below the quadratic's time at 15,000 lines.
walk_big() { # walk_big <lines>: the answer, the match being the listing's last line
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/latest_boot_snapshot.sh"
        listing="$(filler "$1")"$'\n''ID 900 gen 9 top level 5 path nvme/root-.20261010T0100'
        latest_boot_snapshot "$listing" root- nvme
        printf '%s' "$LATEST_BOOT_SNAPSHOT"
    )
}
for n in 1500 5000 15000; do
    start=$EPOCHREALTIME
    got="$(walk_big "$n")"
    end=$EPOCHREALTIME
    check "walk, $n lines with the match last: found" "$got" "nvme/root-.20261010T0100"
    # EPOCHREALTIME is "sec.usec": dropping the point leaves microseconds.
    us=$((${end/./} - ${start/./}))
    verdict=fast
    ((us < 5000000)) || verdict="slow: $us us"
    check "walk, $n lines: under 5 s (the quadratic walk took 0.6 s, 9 s and 89 s at these sizes)" "$verdict" "fast"
done
# The word split must neither glob nor lose a field, and must leave the
# caller's globbing setting as it found it.
glob_probe() { # glob_probe <noglob|glob>: "<answer> <globbing after>"
    (
        set -euo pipefail
        # shellcheck source=/dev/null
        source "$WORK/latest_boot_snapshot.sh"
        [[ $1 == noglob ]] && set -f
        listing=$'ID 1 gen 9 top level 5 path *\n\n*\nID 2 gen 9 top level 5 path nvme/root-.20261010T0100\n?'
        cd "$WORK"
        latest_boot_snapshot "$listing" root- nvme
        if [[ $- == *f* ]]; then g=off; else g=on; fi
        printf '%s globbing-%s' "$LATEST_BOOT_SNAPSHOT" "$g"
    )
}
check "walk: glob characters and blank lines in a listing change nothing; globbing was on, stays on" \
    "$(glob_probe glob)" "nvme/root-.20261010T0100 globbing-on"
check "walk: globbing was off, stays off" \
    "$(glob_probe noglob)" "nvme/root-.20261010T0100 globbing-off"

# nqbb (b): a mirror target whose listing WOULD match, full run, live present:
# it is skipped before anything is listed, and nothing on it is written.
check "boot full, mirror, listing that matches, live present: skipped, nothing written" \
    "$(BOOT_MIRRORS=/mnt/t BOOT_PRESENT="/mnt/t/@ /mnt/t/@home" run_boot_subvols "$SHARED") $(snap_calls) $(calls_of 'subvolume delete') $(calls_of '^mv')" \
    "OK|0 updated, 1 skipped 0 0 0"
check "boot full, mirror: the listing is never even read" "$(calls_of 'subvolume list')" "0"
# nqbb (c): full run, live absent: one create from the newest snapshot, no archive.
check "boot full, live absent: created, counted" \
    "$(BOOT_PLAN=$ONE run_boot_subvols "$SHARED")" "OK|1 updated, 0 skipped"
check "boot full, live absent: exactly one create, from the newest snapshot, no delete, rename or archive" \
    "$(boot_seq)" "subvolume snapshot /mnt/t/nvme/root-.20261005T0100_1 /mnt/t/@;"
# nqbb (d): the create itself fails: FAIL, nothing else called.
check "boot full, live absent, the create fails: FAIL, counted" \
    "$(BOOT_FAIL_ON="snapshot /mnt/t/nvme" BOOT_PLAN=$ONE run_boot_subvols "$SHARED")" "FAIL|0 updated, 1 failed"
check "boot full, live absent, the create fails: only the one attempt, nothing deleted or renamed" \
    "$(boot_seq) $(calls_of 'subvolume delete') $(calls_of '^mv')" \
    "subvolume snapshot /mnt/t/nvme/root-.20261005T0100_1 /mnt/t/@; 0 0"
check "boot full, live absent, the create fails: said" \
    "$(said_boot 'Failed to create @ from nvme/root-.20261005T0100_1')" "1"
# nqbb (e): the listing's temp file cannot be made: FAIL for the target, and
# btrfs is never asked for the listing (the plan's own mktemp is the first).
check "boot, the listing's mktemp fails: FAIL for the target, counted once" \
    "$(BOOT_MKTEMP_FAIL_NTH=2 run_boot_subvols "$SHARED")" "FAIL|0 updated, 1 failed"
check "boot, the listing's mktemp fails: said, and no listing, snapshot, delete or rename" \
    "$(said_boot 'Could not make a temp file to list its subvolumes') $(calls_of 'subvolume list') $(snap_calls) $(calls_of 'subvolume delete') $(calls_of '^mv')" \
    "1 0 0 0 0"
check "boot, the plan's mktemp fails: FAIL, nothing asked of btrfs" \
    "$(BOOT_MKTEMP_FAIL_NTH=1 run_boot_subvols "$SHARED") $(boot_btrfs_calls)" "FAIL|0 updated, 1 failed none"

echo ""
echo "passed=$pass failed=$fail"
[[ -z "$not_run" ]] || echo "NOT RUN: $not_run"
if ((fail == 0)); then
    echo "EARLY-EXIT READERS SUITE GREEN"
    exit 0
fi
exit 1
