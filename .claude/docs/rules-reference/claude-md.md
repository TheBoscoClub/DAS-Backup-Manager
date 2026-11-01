<!-- markdownlint-disable-file MD024 MD025 -- verbatim excerpts moved from .claude/rules/; reformatting would break the lossless-move proof -->
# CLAUDE.md detail — reference (NOT auto-loaded)

Holds lines trimmed from `CLAUDE.md`, same headings.

# Rust is built by CMake into build/cargo-target/; never a bare `cargo build`

<!-- full original lines moved from the rule file, 2026-11-01 -->
# Rust is built by CMake into build/cargo-target/; never a bare `cargo build`

# Install prefix defaults to /usr (not CMake's /usr/local); packaging passes it explicitly

## Detailed Rules

<!-- full original lines moved from the rule file, 2026-11-01 -->
- `esp-safety.md` — **CRITICAL** — recovery-drive ESP identification, independence and incident
- `build.md` — CMake, Qt6/KF6, C++20, Rust build conventions
- `backup.md` — btrbk, DAS, boot archival, email, exit codes, locks, supervisor interaction
- `fail-silent.md` — which error suppressions are legitimate here, and which are always defects
- `test-safety.md` — a suite must never take the host down: task cap, no self-calling stubs
- `tracking.md` — where a new bd issue goes (phase epics) and the orphan check
- `local-host-identifiers.md` — (gitignored, this machine only) real serials/UUIDs behind the tracked placeholders
- `local-installation.md` — (gitignored, this machine only) this installation's targets, enclosure, ESPs, mail, supervisor notes
