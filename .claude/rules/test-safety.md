# Test Safety — This Repository's Task Cap

The general rules (no self-calling stubs, bounded suites, no blind re-runs, killing a runaway) are
global for the maintainer's sessions. The incident started here: a PATH stub named like a command it called
forked until the host ran out of memory; its write-up is `.claude/docs/rules-reference/test-safety.md`
(NOT auto-loaded). bd `DAS-Backup-Manager-29sv`.
Installation specifics: `.claude/rules/local-installation.md` (gitignored, this machine only).

## Here

1. **Every `tests/test_*.sh` sources `tests/lib/bound.sh` straight after its `set` line** — the
   task cap (soft `RLIMIT_NPROC` = the user's tasks + 1024, `DAS_TEST_TASK_HEADROOM`).
   `tests/test_suite_bounds.sh` (ctest `shell-suite-bounds`) fails a suite that does not, and
   proves the cap stops a self-calling stub. A new suite copies the three lines from any existing one.
2. **A suite run by hand still runs in its own scope**:
   `systemd-run --user --scope --quiet -p TasksMax=600 bash tests/<suite>.sh`. The vm suite runs alone.
3. **Trimmed copies** (`tests/zz_trim_*.sh`) carry the cap from the suite's header, live only for
   the run, and are never committed.
