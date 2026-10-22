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
    # Inside a PID namespace of its own (~/.claude/bin/bounded-test since
    # 2026-10-22), ps sees only the run's tasks while the kernel still counts
    # every task of the real user: the measured cap would sit below what the
    # user already runs, and the suite's first fork would fail. There the
    # task cap is the cgroup's pids.max (bounded-test's TasksMax); a namespace
    # without a finite one is refused, never run uncapped. 4026531836 is the
    # kernel's fixed inode for the initial PID namespace (PROC_PID_INIT_INO).
    local ns cg max
    ns=$(stat -L -c %i /proc/self/ns/pid) || {
        echo "bound.sh: cannot read this process's PID namespace: refusing to run uncapped" >&2
        exit 2
    }
    if [[ "$ns" != 4026531836 ]]; then
        cg=$(sed -n 's/^0:://p' /proc/self/cgroup)
        max=$(cat "/sys/fs/cgroup${cg}/pids.max" 2>/dev/null) || max=
        if [[ "$max" =~ ^[0-9]+$ ]]; then
            return 0
        fi
        echo "bound.sh: in a PID namespace whose cgroup has no finite pids.max ('${max:-unreadable}'): refusing to run uncapped" >&2
        exit 2
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
