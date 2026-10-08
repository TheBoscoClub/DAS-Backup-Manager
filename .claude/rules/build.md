# Build Conventions

## Single Canonical Source — Project-Specific Paths
Policy: `~/.claude/rules/development-tools.md`. Canonical locations here:
- **Version**: `CMakeLists.txt` `project(VERSION ...)`; Rust from `Cargo.toml`; GUI via `target_compile_definitions(BTRDASD_VERSION="${CMAKE_PROJECT_VERSION}")`.
- **btrbk config**: `/etc/btrbk/btrbk.conf` (canonical); `/etc/das-backup/btrbk.conf` is a symlink; `/usr/lib/das-backup/config/btrbk.conf` is a reference template only.
- **Binaries**: cmake installs to `/usr/bin/` and `/usr/libexec/`; symlinks only if other paths need them.
- **Build artifacts**: `cmake --build build` only (uses `build/cargo-target/`); never bare `cargo build`.

## C++20 Standards
- Use concepts, ranges, std::format, designated initializers; `std::filesystem` for all paths
- `-Wall -Wextra -Wpedantic -Werror`

## CMake
- Minimum 3.25 (Qt6 support); host 4.4.4; ECM for KDE integration
- `target_link_libraries` with correct PRIVATE/PUBLIC
- RelWithDebInfo for dev, Release for install

## Qt6 / KF6
- Minimum Qt 6.6 / KF 6.0 (`gui/CMakeLists.txt`); host Qt 6.11.2, KF 6.30.0
- KXmlGuiWindow for main window (KDE HIG); KAboutData for metadata; KIO for file ops (restore)
- New-style `connect(&obj, &Class::signal, ...)`

## Rust (buttered_dasd library + btrdasd CLI)
- Rust 2024 edition
- `buttered_dasd` exports 23 public modules (`adopt`, `backup`, `btrbk_conf`, `caldate`, `config`, `db`, `doctor`, `expire`, `forget`, `fsutil`, `health`, `indexer`, `maintenance`, `mount`, `progress`, `reconcile`, `recovery_os`, `report`, `restore`, `scanner`, `schedule`, `scrub`, `subvol`); `setup/` is binary-only. Verify with `grep -c '^pub mod ' indexer/src/lib.rs`, don't trust this line
- `LazyLock<Regex>` for compile-once regex (not per-call `Regex::new()`)
- Release profile: `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = true`
- All DB access through `db::Database` with prepared statements
- `NewBackupRun` struct pattern for functions with >7 parameters
- **Mutation testing gates every push** (`.github/workflows/mutants.yml`, scored by `.github/scripts/mutants-gate.py`) plus a weekly full scope (files listed in the workflow). New code that spawns a command or prompts must go through a test-drivable seam (`CommandRunner`, `Prompter`, the `*_with` cores), or its mutants survive. Local run: header of `indexer/.cargo/mutants.toml` — `--in-place`, crate-relative diff, on a copy of the tree, never `-- --lib`

## SQLite
- SQLite 3.53.2, bundled by rusqlite (libsqlite3-sys 0.38.2), with FTS5
- Prepared statements only (no string concatenation); WAL journal mode; PRAGMA optimize on close
