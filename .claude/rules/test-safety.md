# Test Safety — This Repository's Task Cap

The general rules (no self-calling stubs, bounded suites, no blind re-runs, killing a runaway) are
global for the maintainer's sessions. Incident write-up and detail: `.claude/docs/rules-reference/test-safety.md`
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
4. **No test reaches a real mailer** (bd `DAS-Backup-Manager-1x76`). Rust: a report send takes a
   `report::Mailer`; tests pass a recording one, and `report::MAILX` refuses in every test build — feature
   `test-overrides`, which the crate's dev-dependency on itself gives every target `cargo test` builds; never
   remove it there. A test never sends through `MAILX` to check what it is (`dyn Any` downcast instead), and a
   test config that reaches a mailer names an unreachable relay (`127.0.0.1:1`), never port 25.
   Shell: a suite that runs a real send stubs `mailx` on an `env -i` `PATH`; a suite that extracts functions
   stubs `send_report` itself or never defines it. Proofs: `indexer/tests/test_builds_never_mail.rs` and the unit tests named `*never_spawns_mailx` (the refusal);
   `tests/test_report_mail.sh` (CTest `shell-report-mail`: a built `btrdasd` mails, and both shipped binaries carry
   the live send and not the refusal — texts it reads from the source, never copies; a stale copy passes silently).
