# shellcheck shell=bash
# Sourced first by every shell suite (tests/test_suite_bounds.sh fails a suite
# that does not). A suite stubs commands on PATH, and a stub that calls its own
# name finds itself and forks without end. On 2026-10-13 one did: ~99k
# processes, 135 GB of RAM and ~121 GB of swap, until systemd-oomd killed the
# whole tmux scope -- every Claude session in it -- and scx_lavd died for want
# of memory (.claude/docs/rules-reference/test-safety.md). The user's process
# limit was 514581, which bounds nothing.
#
# This lowers the soft RLIMIT_NPROC to the tasks this user runs now plus a
# headroom, for the suite and everything it starts: a runaway stops at the cap
# with fork failures (the suite fails, loudly), ~1.5 GB at the default instead
# of all memory. RLIMIT_NPROC counts every task (thread) of the real user, so
# the cap is measured, not fixed. Root is exempt from RLIMIT_NPROC: there the
# suite says so and runs uncapped.
das_test_bound() {
    local headroom=${DAS_TEST_TASK_HEADROOM:-1024} listing now cap soft
    if ((EUID == 0)); then
        echo "bound.sh: running as root, which RLIMIT_NPROC does not bind: no task cap" >&2
        return 0
    fi
    # A ps that failed is never "no tasks": the cap would then be the headroom
    # alone, below what the user already runs, and every fork would fail --
    # loud, but for the wrong reason. Refuse instead.
    listing=$(ps -L -U "$UID" --no-headers) || {
        echo "bound.sh: cannot count this user's tasks (ps failed): refusing to run uncapped" >&2
        exit 2
    }
    now=$(wc -l <<<"$listing")
    cap=$((now + headroom))
    soft=$(ulimit -Su)
    if [[ "$soft" == unlimited ]] || ((cap < soft)); then
        ulimit -Su "$cap"
    fi
}
das_test_bound
