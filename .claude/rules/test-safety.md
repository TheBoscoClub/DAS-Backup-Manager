# Test Safety — A Suite Must Never Take The Host Down

Incident, timeline and proofs: `.claude/docs/rules-reference/test-safety.md` (NOT auto-loaded).
bd `DAS-Backup-Manager-29sv` (sentinel side: `cachyos-sentinel-3ze4`). On 2026-10-13 a PATH stub that called its own name forked 98,877
processes; systemd-oomd killed the whole tmux scope (two Claude sessions with it) and scx_lavd died.

## Rules

1. **A PATH stub never invokes its own name.** Don't stub what the code under test can safely run
   for real; a pass-through stub calls the real binary by absolute path (`/usr/bin/mkdir`).
2. **Every `tests/test_*.sh` sources `tests/lib/bound.sh` straight after its `set` line** — the task
   cap. `tests/test_suite_bounds.sh` (ctest `shell-suite-bounds`) fails a suite that does not.
3. **Stubbed sub-scripts run under `timeout`.**
4. **A new or changed suite run by hand runs in its own scope**:
   `systemd-run --user --scope --quiet -p TasksMax=600 bash tests/<suite>.sh`. The vm suite still
   runs alone.
5. **A suite that stops at the same line twice is a finding.** Read its process tree
   (`pstree -ap <pid>`) before re-running it; never re-run blind.
6. **Kill a runaway by exact name** (`pkill -x <name>`) or a bracketed pattern — an unbracketed
   `pkill -f` matches the shell running it.
