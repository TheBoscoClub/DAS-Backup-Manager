#!/bin/bash
# tests/test_create_snapshot_dirs.sh
#
# Regression test for DAS-Backup-Manager-0p2.
#
# create_snapshot_dirs() in scripts/backup-run.sh used to do:
#   mkdir -p "${SOURCE_SNAPSHOT_DIRS[$label]}"
# which is relative-path mkdir — under systemd cwd is /, so the directory
# silently lands at /.btrbk-snapshots instead of /.btrfs-hdd/.btrbk-snapshots.
# btrbk then can't find a snapshot_dir for the source and skips it, surfacing
# only "WARNING: ... Failed to fetch subvolume detail" in the journal and
# "Status: FAILURES DETECTED" in the emailed report. 7 consecutive nightly
# runs (2026-05-17 → 2026-05-23) failed for this reason before it was caught.
#
# v4.2.3 fix: compose ${SOURCE_VOLUMES[label]}/${snapshot_dir} when the
# snapshot_dir is relative; respect absolute paths as-is; warn-skip when the
# source has no known volume; surface mkdir failures.
#
# This test extracts create_snapshot_dirs() from the live backup-run.sh by
# `sed`, stubs the rest of the script's globals/logging, and asserts that the
# function (a) lands every dir under its volume, (b) creates nothing under /.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/backup-run.sh"

if [[ ! -f "$SCRIPT" ]]; then
    echo "FAIL: cannot find $SCRIPT" >&2
    exit 1
fi

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

mkdir -p \
    "$WORKDIR/vol-nvme" \
    "$WORKDIR/vol-ssd" \
    "$WORKDIR/vol-hdd" \
    "$WORKDIR/vol-hdd/ClaudeCodeProjects"

declare -A SOURCE_VOLUMES=()
declare -A SOURCE_SNAPSHOT_DIRS=()
SOURCE_VOLUMES[nvme]="$WORKDIR/vol-nvme"
SOURCE_VOLUMES[ssd]="$WORKDIR/vol-ssd"
SOURCE_VOLUMES[hdd-media]="$WORKDIR/vol-hdd"
SOURCE_VOLUMES[hdd-projects]="$WORKDIR/vol-hdd"
SOURCE_VOLUMES[abs-source]="$WORKDIR/vol-nvme"
SOURCE_VOLUMES[orphan]=""

SOURCE_SNAPSHOT_DIRS[nvme]=".btrbk-snapshots"
SOURCE_SNAPSHOT_DIRS[ssd]=".btrbk-snapshots"
SOURCE_SNAPSHOT_DIRS[hdd-media]=".btrbk-snapshots"
SOURCE_SNAPSHOT_DIRS[hdd-projects]="ClaudeCodeProjects/.btrbk-snapshots"
SOURCE_SNAPSHOT_DIRS[abs-source]="$WORKDIR/explicit-abs"
SOURCE_SNAPSHOT_DIRS[orphan]=".btrbk-snapshots"
SOURCE_SNAPSHOT_DIRS[blank]=""

log_info() { echo "[INFO] $*"; }
log_warn() { echo "[WARN] $*"; }

# Source the function out of the live script
eval "$(sed -n '/^create_snapshot_dirs() {/,/^}/p' "$SCRIPT")"

cd /
create_snapshot_dirs

EXPECTED=(
    "$WORKDIR/vol-nvme/.btrbk-snapshots"
    "$WORKDIR/vol-ssd/.btrbk-snapshots"
    "$WORKDIR/vol-hdd/.btrbk-snapshots"
    "$WORKDIR/vol-hdd/ClaudeCodeProjects/.btrbk-snapshots"
    "$WORKDIR/explicit-abs"
)
fail=0
echo
echo "=== Assertions ==="
for d in "${EXPECTED[@]}"; do
    if [[ -d "$d" ]]; then
        echo "  PASS: $d exists"
    else
        echo "  FAIL: $d does NOT exist"
        fail=1
    fi
done

# The buggy version would create dirs like /.btrbk-snapshots
# (the test stages no real volume there, so any leak is unambiguous).
if [[ -e /.btrbk-snapshots ]]; then
    echo "  FAIL: /.btrbk-snapshots leaked to filesystem root"
    fail=1
else
    echo "  PASS: no stray /.btrbk-snapshots at filesystem root"
fi

if [[ $fail -eq 0 ]]; then
    echo
    echo "OK — create_snapshot_dirs lands each dir on its source volume."
else
    echo
    echo "FAILED — create_snapshot_dirs regression." >&2
fi
exit $fail
