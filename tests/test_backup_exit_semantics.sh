#!/bin/bash
# shellcheck disable=SC2034,SC2016,SC2329
# SC2034: OP_STATUS is read by the code fragment extracted from backup-run.sh.
# SC2016: the sed address is single-quoted on purpose — it matches literal
#   ${OP_STATUS[...]} text in the source, so expanding it would defeat it.
# SC2329: log_error is a stub called by the extracted fragment, not by us.
#
# tests/test_backup_exit_semantics.sh
#
# Regression test for bd DAS-Backup-Manager-nsp finding c1.
#
# main()'s last statement used to be `SCRIPT_COMPLETED="true"`, an assignment,
# so the process exit code was that assignment's status -- always 0. btrbk could
# fail outright at 03:00 and `systemctl status das-backup.service` stayed green,
# the journal showed a clean exit, and cachyos-sentinel never retried because it
# only acts on units in `failed` state.
#
# The replacement is a DELIBERATE code following the split bd 18p established
# for the scrub engine:
#     exit 0        the run EXECUTED (per-target failures go to email/DB/log)
#     exit nonzero  the run could NOT execute -- here, btrbk itself failed
#
# This test pins both directions. It is the failure direction that matters: an
# exit code that is always 0 passes any test that only ever checks success.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$REPO_ROOT/scripts/backup-run.sh"

pass=0; fail=0
ok()  { echo "  PASS: $1"; pass=$((pass+1)); }
bad() { echo "  FAIL: $1"; fail=$((fail+1)); }

# Extract the exit-decision tail of main() and drive it with a stubbed OP_STATUS.
# Extracting rather than re-typing it means the test breaks if someone reverts
# the decision to an assignment.
DECISION="$(sed -n '/^    if \[\[ "\${OP_STATUS\[btrbk\]:-OK}" == "FAIL" \]\]; then$/,/^    return 0$/p' "$SRC")"

if [[ -z "$DECISION" ]]; then
    echo "HARNESS BROKEN: could not extract the exit decision from $SRC"
    echo "If main() no longer ends in an explicit return, that is finding c1 regressing."
    exit 2
fi

run_decision() {   # run_decision <btrbk-status> ; echoes the resulting status
    local btrbk_status="$1"
    (
        set +e
        declare -A OP_STATUS=([btrbk]="$btrbk_status")
        log_error() { :; }
        eval "main_tail() { $DECISION
}"
        main_tail
        echo "$?"
    )
}

echo "== SUCCESS direction: a run that executed exits 0 =="
rc="$(run_decision OK)"
if [[ "$rc" == "0" ]]; then
    ok "btrbk OK -> exit 0 (got $rc)"
else
    bad "btrbk OK -> expected 0, got $rc"
fi

echo "== FAILURE direction: btrbk failing must NOT exit 0 =="
rc="$(run_decision FAIL)"
if [[ "$rc" == "0" ]]; then
    bad "btrbk FAIL -> exit 0. This is finding c1: a failed backup reports green to systemd."
else
    ok "btrbk FAIL -> exit $rc (nonzero, so the unit goes failed and is visible)"
fi

echo "== STRUCTURAL: main() must not end on an assignment =="
# The original defect was invisible precisely because the last line looked
# deliberate. Assert on the shape, not just the behaviour.
last_stmt="$(awk '/^main "\$@"$/{exit} {prev2=prev1; prev1=$0} END{print prev2}' "$SRC")"
if [[ "$last_stmt" =~ ^[[:space:]]*[A-Z_]+=\" ]]; then
    bad "main() ends on an assignment ('$last_stmt') — exit code is accidental again"
else
    ok "main() does not end on a bare assignment"
fi

echo
echo "passed=$pass failed=$fail"
[[ $fail -eq 0 ]] || exit 1
echo "BACKUP EXIT SEMANTICS SUITE GREEN"
