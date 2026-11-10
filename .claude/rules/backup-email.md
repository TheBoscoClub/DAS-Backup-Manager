---
paths:
  - "scripts/backup-run.sh"
  - "indexer/src/report.rs"
  - "indexer/src/scrub.rs"
  - "indexer/src/backup.rs"
  - "indexer/src/config.rs"
  - "tests/test_report_mail.sh"
  - "indexer/tests/test_builds_never_mail.rs"
---

# Backup System Rules — Email Reports

Detail per section: `.claude/docs/rules-reference/backup.md` (same headings, NOT auto-loaded) — **read that section before changing anything this file governs.** Split out of `backup.md` 2026-11-09; loads only when a matching file is read or edited.

## Email Reports

- **THIS PROJECT HOLDS NO MAIL CREDENTIAL and must never acquire one.** Reports go unauthenticated and plain (no TLS) over
  loopback to a local relay (`127.0.0.1:25`), which authenticates upstream by **envelope sender**. Never
  copy a relay or provider key into any project or system config.
- Both consumers read `[email]` from `config.toml`: `send_report()` in `scripts/backup-run.sh` and
  `send_email_report_with_kind()` in `indexer/src/report.rs`. `[email].from` selects the relay
  credential, so it must be the relay's configured sender (e.g. `das-backup@example.org`). `DAS_REPORT_TO` / `DAS_REPORT_FROM` override for testing.
- **Every Rust report send goes through `report::Mailer`**: production sends through `report::MAILX`
  (`SystemBackupHost::new` and `run_scrub_pass` take no mailer, so no caller can pass another), tests a
  recording mailer; nothing else spawns `mailx`, and in a test build `MAILX` refuses before it spawns;
  neither shipped binary (the D-Bus helper included) carries the refusal (bd `1x76`, `test-safety.md`).
- **`smtp-auth=none` is REQUIRED on every mailx invocation**, or s-nail aborts with exit 4.
- **Never redirect mailx stderr to `/dev/null`.**
- **Every mailx send is bounded** (`timeout -k 10 60`) and runs with the lock fds closed, so a relay that
  keeps trickling bytes costs the report, not the run.
- The report is written to `$LAST_REPORT` before any send; a failed write is logged and the not-emailed
  lines name the journal. With email off, an unsavable report is a FAIL
  (exit 3, `report:` in the row). **One rule on both paths** (bd `dlpr`): failed email beside a saved
  report is a WARN, run status stands; saved nowhere and mailed nowhere is a FAIL.
- Diagnose with `journalctl -u das-backup`, `journalctl -u postfix`, `mailq`. `status=sent` means the
  provider accepted it, not that it reached the inbox.
