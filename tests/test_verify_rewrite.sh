#!/bin/bash
# backup-verify.sh survives a rewrite of its own file while it loads its
# configuration (bd DAS-Backup-Manager-896, item 3).
#
# Bash reads a script as it runs, command by command, from the offset where
# the last one ended. A copy over the installed script in place (what a plain
# `cp` and `btrdasd setup --upgrade` do) that lands while a top-level command
# is running makes bash read the NEW file's bytes next. tests/test_script_rewrite.sh
# proves the last line `main "$@"; exit $?` covers the time inside main(); the
# configuration load and the drive map ran at the top level before it, and were
# not covered. Here a stub `btrdasd config dump-env` blocks inside that load,
# the script file is rewritten in place with a different, longer script, and
# the stub is released: the run must end exactly as an undisturbed run does,
# and nothing of the new file may run.
#
# Run as a user, so check_root refuses after the load (exit 1): the load is all
# this needs, and no drive, lock or btrbk is touched. Writes only beneath a
# mktemp directory.

set -uo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(dirname "$here")"
work="$(mktemp -d)"
trap 'rm -rf "${work:?}"' EXIT

if [[ "$(id -u)" == 0 ]]; then
    echo "SKIP running as root: check_root would not refuse after the load"
    exit 0
fi

fails=0
check() {
    if [[ "$2" == "$3" ]]; then
        echo "ok   $1"
    else
        echo "FAIL $1"
        echo "       got:  $2"
        echo "       want: $3"
        fails=$((fails + 1))
    fi
}

# The stub: `config dump-env` signals, waits for the go file, then prints a
# configuration with no targets.
cat >"$work/btrdasd" <<'STUB'
#!/bin/bash
dir="$(dirname "${BASH_SOURCE[0]}")"
: >"$dir/blocked"
for _ in $(seq 1 600); do
    [[ -e "$dir/go" ]] && break
    sleep 0.05
done
echo 'DAS_TARGET_COUNT=0'
STUB
chmod +x "$work/btrdasd"

# run <rewrite|plain>: "exit status|lines of the new file that ran" of one run
# of a scratch copy of the script, rewritten in place while the stub blocks,
# or not.
run() {
    rm -f "${work:?}/blocked" "${work:?}/go"
    cp "$repo/scripts/backup-verify.sh" "$work/script.sh"
    (
        BTRDASD_BIN="$work/btrdasd" bash "$work/script.sh" --quick >"$work/out" 2>&1
        echo "$?" >"$work/rc"
    ) &
    local pid=$!
    for _ in $(seq 1 200); do
        [[ -e "$work/blocked" ]] && break
        sleep 0.05
    done
    if [[ "$1" == rewrite ]]; then
        # Same inode, truncated then written, as a plain `cp` does: a different
        # and longer script, every line a command that names itself.
        for _ in $(seq 1 400); do echo 'echo REWRITE-RAN'; done >"$work/script.sh"
    fi
    : >"$work/go"
    wait "$pid"
    printf '%s|%s' "$(cat "$work/rc")" "$(grep -c REWRITE-RAN "$work/out")"
}

plain="$(run plain)"
check "undisturbed: refused as a non-root user, nothing of a rewrite ran" "$plain" "1|0"
check "rewritten in place during the configuration load: ends as the undisturbed run" \
    "$(run rewrite)" "$plain"

if ((fails > 0)); then
    echo "VERIFY REWRITE SUITE RED ($fails failed)"
    exit 1
fi
echo "VERIFY REWRITE SUITE GREEN"
