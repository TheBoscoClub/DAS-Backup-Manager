#!/bin/bash
# refresh_local_docs from scripts/backup-run.sh, against a stub btrdasd (bd
# DAS-Backup-Manager-o77b). Both directions: a refresh that ran records OK with
# its count, a dry run only checks, and a refused document, a failure or an
# exit only a check may give is a WARN that never stops the run.
set -euo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK:?}"' EXIT

extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in refresh_local_docs record_op; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done

# shellcheck disable=SC2329  # called by the eval-extracted functions
log_info() { :; }
# shellcheck disable=SC2329
log_warn() { echo "WARN: $*" >>"$WORK/log"; }
declare -A OP_STATUS=()
# shellcheck disable=SC2034  # read by the functions eval-extracted above
DAS_CONFIG="$WORK/config.toml"
BTRDASD_BIN="$WORK/btrdasd"

# Stub: prints $WORK/out and exits with $WORK/rc; records its arguments.
cat >"$BTRDASD_BIN" <<'STUB'
#!/bin/bash
here="$(dirname "$0")"
echo "$*" >"$here/args"
cat "$here/out"
exit "$(cat "$here/rc")"
STUB
chmod +x "$BTRDASD_BIN"

pass=0 fail=0
check() { # description got want
    if [[ "$2" == "$3" ]]; then
        pass=$((pass + 1))
    else
        echo "FAIL: $1: got [$2], want [$3]"
        fail=$((fail + 1))
    fi
}
run() { # mode rc out
    OP_STATUS=()
    : >"$WORK/log"
    printf '%s' "$3" >"$WORK/out"
    echo "$2" >"$WORK/rc"
    refresh_local_docs "$1"
}

run real 0 $'current      /d/a.md\nupdated      /d/b.md\nupdated      /d/c.md\n'
check "refreshed: OK" "${OP_STATUS[local_docs]}" "OK"
check "refreshed: counts the updated documents" "${OP_STATUS[local_docs_detail]}" "2 updated"
check "refreshed: writes (no --check)" "$(cat "$WORK/args")" "docs refresh --config $WORK/config.toml"
check "refreshed: no warning" "$(cat "$WORK/log")" ""

run dryrun 1 $'would update /d/b.md\n'
check "dry run: checks only" "$(cat "$WORK/args")" "docs refresh --config $WORK/config.toml --check"
check "dry run: a change it would make is OK" "${OP_STATUS[local_docs]}" "OK"
check "dry run: says what would change" "${OP_STATUS[local_docs_detail]}" "dry run: 1 would be updated"

run dryrun 0 $'current      /d/a.md\n'
check "dry run, all current: OK" "${OP_STATUS[local_docs_detail]}" "dry run: 0 would be updated"

run real 2 $'REFUSED      /d/a.md: line 3: a marker with no closing <!-- /btrdasd:subvolumes -->\n'
check "refused: WARN" "${OP_STATUS[local_docs]}" "WARN"
check "refused: names the exit" "${OP_STATUS[local_docs_detail]%% —*}" "local document refresh exited 2"
check "refused: logged" "$(cat "$WORK/log")" "WARN: Local recovery documents not refreshed (exit 2)"

run real 1 ""
check "a real run's exit 1 (only a check gives it): WARN" "${OP_STATUS[local_docs]}" "WARN"

run real 127 "stub: not found"
check "a failure: WARN" "${OP_STATUS[local_docs]}" "WARN"
# A document never stops a backup: the function returns 0 whatever happened.
rc=0
refresh_local_docs real || rc=$?
check "a failure never stops the run" "$rc" "0"

echo "local docs refresh: $pass passed, $fail failed"
[[ $fail -eq 0 ]]
