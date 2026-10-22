#!/bin/bash
# Every shell suite runs under a task cap (tests/lib/bound.sh), and the cap
# holds. On 2026-10-13 a PATH stub that called its own name (pbin/mkdir running
# `mkdir -p "$@"`) forked ~99k processes until systemd-oomd killed the whole
# tmux scope (.claude/docs/rules-reference/test-safety.md).
#
#   1. every tests/test_*.sh sources lib/bound.sh before anything else runs,
#      seen refusing a suite that does not;
#   2. the self-calling stub, depth-limited so it ends on its own, runs to its
#      full depth uncapped and is stopped short under the cap.
set -uo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
T=$(mktemp -d)
trap 'rm -rf "${T:?}"' EXIT
passes=0 fails=0
check() { # <name> <got> <want>
    if [[ "$2" == "$3" ]]; then
        echo "ok   $1"; passes=$((passes + 1))
    else
        echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1))
    fi
}

# Suites in <dir> whose first command is not followed at once by the source
# line: the cap must be in force before a suite stubs or starts anything.
unbounded() {
    local f first next
    for f in "$1"/test_*.sh; do
        first=$(grep -n -m1 -vE '^(#|$)' "$f" | cut -d: -f1)
        next=$(sed -n "$((first + 1)),$((first + 3))p" "$f")
        # shellcheck disable=SC2016 # matched as written
        grep -xF 'source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"' <<<"$next" >/dev/null ||
            printf '%s\n' "${f##*/}"
    done
}

echo "--- every suite sources the cap first"
check "every tests/test_*.sh is capped" "$(unbounded "$HERE")" ""
mkdir -p "$T/suites"
# shellcheck disable=SC2016 # written as a suite would carry it
printf '#!/bin/bash\n# a suite\nset -euo pipefail\nPATH="$T/bin:$PATH"\n' >"$T/suites/test_uncapped.sh"
# shellcheck disable=SC2016 # written as a suite would carry it
printf '#!/bin/bash\nset -euo pipefail\n# shellcheck source=/dev/null\nsource "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"\n' >"$T/suites/test_capped.sh"
check "a suite without the cap is named" "$(unbounded "$T/suites")" "test_uncapped.sh"

echo "--- the self-calling stub: runs away uncapped, stopped under the cap"
# Root is not bound by RLIMIT_NPROC (CI's jobs run as root in an Arch
# container), so as root the stub runs as nobody: the cap is observed on
# every runner, never skipped.
as_runner=()
if ((EUID == 0)); then
    if command -v setpriv >/dev/null && id nobody >/dev/null 2>&1; then
        as_runner=(setpriv --reuid=nobody --regid=nobody --clear-groups)
        chmod 1777 "$T"
    else
        check "root can drop to nobody to observe the cap (setpriv, user nobody)" "no" "yes"
    fi
fi
if ((EUID != 0)) || ((${#as_runner[@]} > 0)); then
    # The shape of 2026-10-13's stub, with a depth limit so the uncapped run
    # ends by itself: each level waits on the next, so DEPTH tasks are alive
    # at once at the bottom.
    DEPTH=300
    mkdir -p "$T/bin"
    cat >"$T/bin/mkdir" <<'STUB'
#!/bin/sh
d=${STUB_DEPTH:-0}
echo "$d" >"$STUB_LOG"
[ "$d" -ge "$STUB_MAX" ] && exit 0
STUB_DEPTH=$((d + 1)) mkdir -p "$@"
STUB
    chmod 755 "$T/bin" "$T/bin/mkdir"
    reached() { # <headroom or "none">: the deepest level the stub reached
        : >"$T/depth"
        chmod 666 "$T/depth"
        # A fresh bash sources the cap itself, as the user it runs as.
        # shellcheck disable=SC2016 # expanded by the inner bash
        "${as_runner[@]}" bash -c '
            if [[ "$1" != none ]]; then DAS_TEST_TASK_HEADROOM=$1 source "$2"; fi
            PATH="$3/bin:$PATH" STUB_LOG="$3/depth" STUB_MAX=$4 timeout 120 mkdir -p "$3/x"
        ' _ "$1" "$HERE/lib/bound.sh" "$T" "$DEPTH" >/dev/null 2>&1
        cat "$T/depth"
    }
    check "uncapped, the stub reaches its full depth (it is a runaway)" "$(reached none)" "$DEPTH"
    got=$(reached 50)
    check "capped at 50 more tasks, the stub is stopped short" "$((got < DEPTH / 2))" "1"
fi

echo "--- in a PID namespace of its own: the cgroup's pids.max is the cap, or the suite refuses"
# ~/.claude/bin/bounded-test runs every suite in its own PID namespace, where
# ps sees only the run but the kernel counts all of the user's tasks: a cap
# measured there sat below the user's live count and the first fork failed
# (2026-10-22, bd DAS-Backup-Manager-afid). Needs a user manager (a scope sets
# pids.max) and passwordless sudo (root makes the namespace); CI's root
# container has neither, and the section says it did not run.
if ((EUID != 0)) && systemctl --user show -p Version --value >/dev/null 2>&1 &&
    sudo -n true 2>/dev/null; then
    in_ns() { # <TasksMax>: bound.sh's exit status, then a fork, in a fresh PID namespace
        # shellcheck disable=SC2016 # expanded by the inner bash
        systemd-run --user --scope --quiet -p TasksMax="$1" \
            sudo -n unshare --pid --fork --mount-proc \
            setpriv --reuid="$UID" --regid="$(id -g)" --init-groups \
            bash -c 'source "$1"; true & wait && echo forked' _ "$HERE/lib/bound.sh" 2>/dev/null
        echo "rc=$?"
    }
    check "finite pids.max: the suite runs and forks" "$(in_ns 500 | tr '\n' ' ')" "forked rc=0 "
    check "no finite pids.max: the suite refuses (exit 2)" "$(in_ns infinity | tr '\n' ' ')" "rc=2 "
else
    echo "NOT RUN: needs a non-root user with a user manager and passwordless sudo"
fi

echo "$passes passed, $fails failed"
((fails == 0)) || exit 1
echo "SUITE-BOUNDS SUITE GREEN"
