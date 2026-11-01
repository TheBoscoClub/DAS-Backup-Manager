<!-- markdownlint-disable-file MD025 MD029 -- verbatim excerpts moved from .claude/rules/; reformatting would break the lossless-move proof -->
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

# Test Safety — This Repository's Task Cap

<!-- full original lines moved from the rule file, 2026-11-01 -->
The general rules (no self-calling stubs, bounded suites, no blind re-runs, killing a runaway) are
global for the maintainer's sessions. The incident started here: a PATH stub named like a command it called
forked until the host ran out of memory; its write-up is `.claude/docs/rules-reference/test-safety.md`
(NOT auto-loaded). bd `DAS-Backup-Manager-29sv`.
Installation specifics: `.claude/rules/local-installation.md` (gitignored, this machine only).

## Here

<!-- full original lines moved from the rule file, 2026-11-01 -->
4. **No test reaches a real mailer** (bd `DAS-Backup-Manager-1x76`: a mutant once mailed through the
   live relay). Rust: a report send takes a `report::Mailer`; tests pass a recording one with a fixed
   `context()`, and `report::MAILX` refuses in every test build — feature `test-overrides`, which the
   crate's dev-dependency on itself gives every target `cargo test` builds; never remove it there.
   `tests/test_builds_never_mail.rs` and the unit tests named `*never_spawns_mailx` prove the refusal;
   `tests/test_report_mail.sh` proves a built `btrdasd` mails, and that both shipped binaries carry the
   live send and not the refusal — texts it reads from the source, never copies (a stale copy passes
   silently). A test never sends through `MAILX` to check what it is (`dyn Any` downcast instead), and a
   test config that reaches a mailer names an unreachable relay (`127.0.0.1:1`), never port 25.
   Shell: a suite that runs a real send stubs `mailx` on an `env -i` `PATH`
   (`test_backup_exit_semantics.sh`, `test_report_mail.sh`); a suite that extracts functions stubs
   `send_report` itself or never defines it.
