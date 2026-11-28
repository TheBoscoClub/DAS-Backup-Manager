#!/bin/bash
# compute_growth_stats from scripts/backup-run.sh (bd DAS-Backup-Manager-sp78.9):
# a growth average it could not compute — no growth log, no entry for the
# mount in the window, under a day of history — is "unknown", never the 0 a
# measured flat target gives. Since bd DAS-Backup-Manager-3gzs.15 it also
# covers record_growth's line (`<ts> <mount> <used> [<total>]`) and the
# reader taking 4-field, mixed and torn lines.
set -euo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK:?}"' EXIT

body="$(sed -n '/^compute_growth_stats() {/,/^}/p' "$SCRIPT")"
[[ -n "$body" ]] || { echo "FAIL: compute_growth_stats not found in backup-run.sh"; exit 1; }
eval "$body"

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }

day=86400
stamp() { date -d "@$(($(date +%s) - $1))" '+%Y-%m-%dT%H:%M:%S'; }

GROWTH_LOG="$WORK/absent.log"
check "no growth log: unknown" "$(compute_growth_stats /mnt/t 1000 7)" "unknown"

GROWTH_LOG="$WORK/growth.log"
printf '%s /mnt/other 100\n' "$(stamp $((3 * day)))" >"$GROWTH_LOG"
check "no entry for the mount: unknown" "$(compute_growth_stats /mnt/t 1000 7)" "unknown"

printf '%s /mnt/t 100\n' "$(stamp 3600)" >"$GROWTH_LOG"
check "under a day of history: unknown" "$(compute_growth_stats /mnt/t 1000 7)" "unknown"

printf '%s /mnt/t 1000\n' "$(stamp $((2 * day + 60)))" >"$GROWTH_LOG"
check "a measured flat target: 0" "$(compute_growth_stats /mnt/t 1000 7)" "0"

printf '%s /mnt/t 1000\n' "$(stamp $((2 * day + 60)))" >"$GROWTH_LOG"
check "measured growth: per day" "$(compute_growth_stats /mnt/t 3000 7)" "1000"

# Since bd DAS-Backup-Manager-3gzs.15 a line carries the target's capacity as a
# 4th field; the used bytes are still the 3rd.
printf '%s /mnt/t 1000 20000\n' "$(stamp $((2 * day + 60)))" >"$GROWTH_LOG"
check "a 4-field line: per day" "$(compute_growth_stats /mnt/t 3000 7)" "1000"

# A log of both shapes: an old 3-field line outside the window, then the
# 4-field line the window starts at.
{
    printf '%s /mnt/t 10\n' "$(stamp $((9 * day)))"
    printf '%s /mnt/other 5\n' "$(stamp $((3 * day)))"
    printf '%s /mnt/t 1000 20000\n' "$(stamp $((2 * day + 60)))"
    printf '%s /mnt/t 2000\n' "$(stamp $((day + 60)))"
} >"$GROWTH_LOG"
check "a mixed 3/4-field log: per day" "$(compute_growth_stats /mnt/t 3000 7)" "1000"

# A torn write (NUL bytes) or a line with no number is skipped, never read as 0.
{
    printf '\0\0\0%s /mnt/t\n' "$(stamp $((3 * day + 60)))"
    printf '%s /mnt/t 1000 20000\n' "$(stamp $((2 * day + 60)))"
} >"$GROWTH_LOG"
check "a line with no used bytes is skipped" "$(compute_growth_stats /mnt/t 5000 7)" "2000"

# --- record_growth: the line it writes -------------------------------------
rg_body="$(sed -n '/^record_growth() {/,/^}/p' "$SCRIPT")"
[[ -n "$rg_body" ]] || { echo "FAIL: record_growth not found in backup-run.sh"; exit 1; }
eval "$rg_body"
# The functions it calls, stubbed on their readings (never df on the host).
mountpoint() { [[ "$2" != /mnt/absent ]]; }
# shellcheck disable=SC2329 # called by the eval'd record_growth
get_used_bytes() { case "$1" in /mnt/a) echo 1000 ;; /mnt/b) echo 2000 ;; /mnt/c) echo "" ;; esac; }
# shellcheck disable=SC2329 # called by the eval'd record_growth
get_total_bytes() { case "$1" in /mnt/a) echo 50000 ;; /mnt/b) echo "" ;; /mnt/c) echo 9 ;; esac; }
GROWTH_LOG="$WORK/rg/growth.log"
# shellcheck disable=SC2034 # read by the eval'd record_growth
ALL_TARGET_MOUNTS=(/mnt/a /mnt/b /mnt/c /mnt/absent)
record_growth
re_ts='[[:digit:]]{4}-[[:digit:]]{2}-[[:digit:]]{2}T[[:digit:]]{2}:[[:digit:]]{2}:[[:digit:]]{2}'
lines="$(cat "$GROWTH_LOG")"
check "record_growth writes one line per read target" "$(wc -l <"$GROWTH_LOG")" "2"
if [[ "$(sed -n 1p "$GROWTH_LOG")" =~ ^${re_ts}\ /mnt/a\ 1000\ 50000$ ]]; then
    echo "ok   record_growth: used and capacity"
else
    echo "FAIL record_growth: used and capacity: got '$lines'"; fails=$((fails + 1))
fi
if [[ "$(sed -n 2p "$GROWTH_LOG")" =~ ^${re_ts}\ /mnt/b\ 2000$ ]]; then
    echo "ok   record_growth: an unread capacity leaves three fields, never a 0"
else
    echo "FAIL record_growth: an unread capacity: got '$lines'"; fails=$((fails + 1))
fi
# What record_growth wrote, compute_growth_stats reads.
check "compute_growth_stats reads record_growth's line" \
    "$(compute_growth_stats /mnt/a 1000 7)" "unknown"

# A df that FAILS (exit 1 through pipefail, as get_*_bytes do when df cannot
# read the mount) costs that reading, never the nightly run: record_growth runs
# under set -e here, as in backup-run.sh, and must reach its end.
get_used_bytes() { case "$1" in /mnt/e) return 1 ;; *) echo 3000 ;; esac; }
get_total_bytes() { case "$1" in /mnt/f) return 1 ;; *) echo 60000 ;; esac; }
GROWTH_LOG="$WORK/rg/growth-fail.log"
# Its own bash: set -e is ignored inside anything on the left of || (even when
# set again there), so a subshell in this script could not show the abort.
survived="$(bash -c "set -euo pipefail
$(declare -f mountpoint get_used_bytes get_total_bytes record_growth)
GROWTH_LOG='$GROWTH_LOG'; ALL_TARGET_MOUNTS=(/mnt/e /mnt/f)
record_growth; echo survived" 2>/dev/null)" || true
[[ -n "$survived" ]] || survived="aborted"
check "a failing df read does not end the run" "$survived" "survived"
if [[ "$(cat "$GROWTH_LOG" 2>/dev/null)" =~ ^${re_ts}\ /mnt/f\ 3000$ ]]; then
    echo "ok   a failed used read writes no line, a failed capacity read three fields"
else
    echo "FAIL a failing df read: got '$(cat "$GROWTH_LOG" 2>/dev/null)'"; fails=$((fails + 1))
fi

if ((fails > 0)); then
    echo "test_growth_stats: $fails FAILED"
    exit 1
fi
echo "test_growth_stats: all passed"
