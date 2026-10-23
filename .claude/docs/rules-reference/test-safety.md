# Test Safety — Reference

Binding rules: `.claude/rules/test-safety.md`. Tracker: bd `DAS-Backup-Manager-29sv`.
The maintainer's host timeline of the incident is local-only (`.claude/docs/local/test-safety.md`, gitignored).

## The incident, in mechanism

A trimmed copy of the recovery-os-vm suite wrote a PATH stub `mkdir` as `#!/bin/sh` + `mkdir -p "$@"`, with the
stub directory first on `PATH`. The stub found itself: every copy started another. Roughly 99,000 processes
exhausted RAM and swap, and the OOM policy killed the whole cgroup holding the session.

### Why each defence failed

1. **The stub called its own name.** A PATH stub is a program named like the real one; any call to that name
   inside it resolves to itself. A pass-through stub must name the real binary by absolute path — or, better,
   not exist: the proof script's `mkdir -p "$top"` was safe to run for real.
2. **Nothing bounded a suite.** `RLIMIT_NPROC` was effectively unlimited; no `timeout`, no cgroup task cap.
3. **A terminal multiplexer protects nothing from a cgroup kill.** The OOM policy kills a whole cgroup; every
   pane and the runaway shared one scope.
4. **The re-run.** A suite that stops at the same line twice is a finding, not a flake. Look at the process
   tree (`pstree -ap <pid>`) before running it again.
5. **The cleanup nearly made it worse.** An unbracketed `pkill -f 'pbin/mkdir'` matched the shell running it.
   Kill by exact name (`pkill -x mkdir`) or with a bracketed pattern (`'[p]bin/mkdir'`).

## What now prevents it

| Layer | Where | Proof |
| --- | --- | --- |
| Stub removed; the proof run under `timeout 30` | `tests/test_recovery_os_vm.sh` | full suite passes |
| Every suite capped at the user's tasks + 1024 (`RLIMIT_NPROC`), sourced before anything else runs | `tests/lib/bound.sh`, sourced by all `tests/test_*.sh` | — |
| The cap is present and holds | `tests/test_suite_bounds.sh`, ctest `shell-suite-bounds` | uncapped, the depth-limited stub reaches 300; capped at +50 it stops short. Seen red with the `ulimit` neutered, and with one suite's source line removed |

`RLIMIT_NPROC` counts every task (thread) of the real user, so the cap is measured at start, not fixed. Root is
exempt; a suite run as root says so and runs uncapped. A new or changed suite run by hand is also wrapped in
`systemd-run --user --scope -p TasksMax=<n>`, which bounds the cgroup and keeps the OOM policy's target narrow.
