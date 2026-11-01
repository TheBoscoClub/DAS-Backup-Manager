# Build Conventions

Installation specifics: `.claude/rules/local-installation.md`; host toolchain versions: `.claude/docs/local/local-installation.md` (both gitignored, this machine only).
Detail behind these rules: `.claude/docs/rules-reference/build.md` (same headings, NOT auto-loaded).

## Single Canonical Source — Project-Specific Paths

Policy: every production file exists as one object at one canonical path; any second path is a symlink. Canonical locations here:

- **Version**: `CMakeLists.txt` `project(VERSION ...)`; Rust from `Cargo.toml`; GUI via `target_compile_definitions(BTRDASD_VERSION="${CMAKE_PROJECT_VERSION}")`.
- **btrbk config**: `/etc/btrbk/btrbk.conf` (canonical); `/etc/das-backup/btrbk.conf` is a symlink; `/usr/lib/das-backup/config/btrbk.conf` is a reference template only.
- **Binaries**: cmake installs to `/usr/bin/` and `/usr/libexec/`; symlinks only if other paths need them. The prefix defaults to `/usr` at the top of `CMakeLists.txt` and every `DESTINATION` is prefix-relative (bd `laj`); never build an absolute destination from `${CMAKE_INSTALL_PREFIX}` at configure time.
- **Build artifacts**: `cmake --build build` only (uses `build/cargo-target/`); never bare `cargo build`.

## C++20 Standards

- Use concepts, ranges, std::format, designated initializers; `std::filesystem` for all paths
- `-Wall -Wextra -Wpedantic -Werror`

## CMake

- Minimum 3.25 (Qt6 support); ECM for KDE integration
- `target_link_libraries` with correct PRIVATE/PUBLIC
- RelWithDebInfo for dev, Release for install

## Qt6 / KF6

- Minimum Qt 6.6 / KF 6.0 (`gui/CMakeLists.txt`)
- KXmlGuiWindow for main window (KDE HIG); KAboutData for metadata; KIO for file ops (restore)
- New-style `connect(&obj, &Class::signal, ...)`

## Rust (buttered_dasd library + btrdasd CLI)

- Rust 2024 edition
- `buttered_dasd` exports its public modules from `indexer/src/lib.rs` (`setup/` is binary-only); list them with `grep '^pub mod ' indexer/src/lib.rs`.
- `LazyLock<Regex>` for compile-once regex (not per-call `Regex::new()`)
- Release profile: `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = true`
- All DB access through `db::Database` with prepared statements
- `NewBackupRun` struct pattern for functions with >7 parameters
- **Mutation testing gates every push** (`.github/workflows/mutants.yml`, scored by `.github/scripts/mutants-gate.py`) plus a weekly full scope. New code that spawns a command or prompts must go through a test-drivable seam (`CommandRunner`, `Prompter`, the `*_with` cores), or its mutants survive. Local run: header of `indexer/.cargo/mutants.toml`.

## SQLite

- SQLite bundled by rusqlite (libsqlite3-sys), with FTS5
- Prepared statements only (no string concatenation); WAL journal mode; PRAGMA optimize on close
