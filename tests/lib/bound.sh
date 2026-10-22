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
#
# The count is the kernel's own, not ps's: inside a PID namespace of its own
# (~/.claude/bin/bounded-test since 2026-10-22) ps sees only the run's tasks
# while the kernel still counts all of the user's, and a cap measured with ps
# sat below the live count, so the suite's first fork failed (bd
# DAS-Backup-Manager-afid). A fork under soft limit N succeeds only while the
# user runs fewer than N tasks; a binary search over N finds the count.

# Whether a fork succeeds under soft RLIMIT_NPROC <n>. coreutils' timeout does
# the fork: it fails at once on EAGAIN, where bash's own fork retries for ~15 s.
das_test_forks_under() {
    (ulimit -Su "$1" && exec timeout 5 true) 2>/dev/null
}

# The tasks this user runs now, as RLIMIT_NPROC counts them; fails if a fork
# fails even at the hard limit (then nothing could be measured, or run).
das_test_user_tasks() {
    local lo=1 hi mid
    hi=$(ulimit -Hu)
    [[ "$hi" == unlimited ]] && hi=$((1 << 22)) # above any pid_max
    das_test_forks_under "$hi" || return 1
    while ((lo < hi)); do
        mid=$(((lo + hi) / 2))
        if das_test_forks_under "$mid"; then hi=$mid; else lo=$((mid + 1)); fi
    done
    # The smallest limit a fork succeeds under is the count plus one; that
    # count includes the probe's own subshell.
    echo $((lo - 1))
}

das_test_bound() {
    local headroom=${DAS_TEST_TASK_HEADROOM:-1024} now cap soft
    if ((EUID == 0)); then
        echo "bound.sh: running as root, which RLIMIT_NPROC does not bind: no task cap" >&2
        return 0
    fi
    # A count that could not be taken is never "no tasks": the cap would then
    # be the headroom alone, below what the user already runs, and every fork
    # would fail -- loud, but for the wrong reason. Refuse instead.
    now=$(das_test_user_tasks) || {
        echo "bound.sh: cannot count this user's tasks (no fork succeeds): refusing to run uncapped" >&2
        exit 2
    }
    cap=$((now + headroom))
    soft=$(ulimit -Su)
    if [[ "$soft" == unlimited ]] || ((cap < soft)); then
        ulimit -Su "$cap"
    fi
}
das_test_bound
