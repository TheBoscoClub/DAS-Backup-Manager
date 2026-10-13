# Test Safety — Reference

Binding rules: `.claude/rules/test-safety.md`. Tracker: bd `DAS-Backup-Manager-29sv`.

## The 2026-10-13 incident

### Sequence (host clock, CDT; journal-confirmed)

| Time | Event |
| --- | --- |
| 13:03:20 | Session `3db8a8fb` runs `tests/zz_trim_provision.sh`, a trimmed copy of `tests/test_recovery_os_vm.sh`. Its new, uncommitted proof tests wrote `pbin/mkdir` as `#!/bin/sh` + `mkdir -p "$@"`, with `pbin` first on `PATH`. The stub found itself: every copy started another |
| 13:05:54 | Kernel: `claude: page allocation failure: order:0` inside `tmux-spawn-69a0bfea….scope` |
| 13:06:30 | `scx_lavd` cannot allocate its per-task records and exits; `scx_loader` retries 5 times, all fail (one segfault). `sched_ext` sits **disabled**, the kernel falls back to its stock scheduler, while `scx_loader` and `scx-autoswitch` keep reporting "running" |
| 13:08:06–36 | `sentinel-core` logs memory state `idle` → `pressure` → `reactive`. It observed; it killed nothing |
| 13:09:45–46 | `systemd-oomd`: RAM 135 GB of 135 GB, ~121 GB swap. Kills the whole `tmux-spawn-69a0bfea…` scope in two passes, 32,981 + 65,896 = **98,877 processes**, almost all `mkdir` |
| 13:15 | The follow-up session (`bda819c8`) re-runs the same trimmed suite without first asking why it had stopped at the same line. It reaches ~30,000 processes before being killed by hand (`pkill -x mkdir`) |
| 13:33:55 | `scxctl restart` (sentinel's investigation) brings `lavd_1.1.3` back; `/sys/kernel/sched_ext/state` reads `enabled` |

KDE showed the oomd kills as "Memory Shortage Avoided … terminated by the Linux kernel". That wording is
systemd-oomd's, not the OOM killer's and not sentinel's.

### Why each defence failed

1. **The stub called its own name.** A PATH stub is a program named like the real one; any call to
   that name inside it resolves to itself. A pass-through stub must name the real binary by absolute
   path — or, better, not exist: the proof script's `mkdir -p "$top"` was safe to run for real.
2. **Nothing bounded a suite.** `RLIMIT_NPROC` was 514,581, effectively none: ~99k processes at
   ~1.4 MB each was already 135 GB plus 121 GB of swap. No `timeout`, no cgroup task cap.
3. **tmux was no protection.** oomd kills a whole cgroup. The tmux server, every pane, both Claude
   sessions and the runaway suite shared one scope, so they went down together. tmux survives a closed
   terminal, not an oomd kill of its own scope.
4. **The re-run.** A suite that stops at the same line twice is a finding, not a flake. Look at the
   process tree (`pstree -ap <pid>`) before running it again.
5. **The cleanup nearly made it worse.** `pkill -f 'pbin/mkdir'` (unbracketed) matched the shell
   running it, and its `-STOP` froze that shell. Kill by exact name (`pkill -x mkdir`) or with a
   bracketed pattern (`'[p]bin/mkdir'`).

## What now prevents it

| Layer | Where | Proof |
| --- | --- | --- |
| Stub removed; the proof run under `timeout 30` | `tests/test_recovery_os_vm.sh`, commit `7ac88be` | full suite 2126 passed, 0 failed |
| Every suite capped at the user's tasks + 1024 (`RLIMIT_NPROC`), sourced before anything else runs | `tests/lib/bound.sh`, sourced by all `tests/test_*.sh` | — |
| The cap is present and holds | `tests/test_suite_bounds.sh`, ctest `shell-suite-bounds` | uncapped, the depth-limited stub reaches 300; capped at +50 it stops short. Seen red with the `ulimit` neutered, and with one suite's source line removed |

`RLIMIT_NPROC` counts every task (thread) of the real user, so the cap is measured at start, not
fixed. Root is exempt from it; a suite run as root says so and runs uncapped. Developers and agents
running a new or changed suite by hand also wrap it in
`systemd-run --user --scope -p TasksMax=<n>`, which bounds the cgroup and keeps oomd's target
narrow.

## Outside this repository (operator-approved 2026-10-13)

- **ccp `b460541`**: each ccp-launched Claude session runs in its own `ccp-claude-<project>-…`
  systemd scope with `TasksMax=8192` (`CCP_CLAUDE_TASKS_MAX`), so a runaway meets fork failures at
  its session's cap and oomd takes that one session, not the tmux server and every pane. bats runs
  the real `systemd-run` with a fake `claude`; seen red without the cap and without the scope. A
  `claude` started by hand outside ccp is still in the terminal's cgroup.
- **Global rule** `~/.claude/rules/test-safety.md`: the same stub can be written in any project.
- **Sentinel**, `cachyos-sentinel-3ze4` (linked both ways with `29sv`): `scx_loader` /
  `scx-autoswitch` report "running" while `sched_ext` is disabled, and no detector fired on the
  runaway or the oomd kill.
