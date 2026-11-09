#!/bin/bash
# compute_growth_stats from scripts/backup-run.sh (bd DAS-Backup-Manager-sp78.9):
# a growth average it could not compute — no growth log, no entry for the
# mount in the window, under a day of history — is "unknown", never the 0 a
# measured flat target gives.
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

if ((fails > 0)); then
    echo "test_growth_stats: $fails FAILED"
    exit 1
fi
echo "test_growth_stats: all passed"
