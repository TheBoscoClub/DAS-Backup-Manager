# Build Conventions

## Single Canonical Source — Project-Specific Paths

See global rule in `~/.claude/rules/development-tools.md` for the full policy. Project-specific canonical locations:

- **Version source of truth**: `CMakeLists.txt` `project(VERSION ...)`. Rust gets it from `Cargo.toml`. GUI gets it via `target_compile_definitions(BTRDASD_VERSION="${CMAKE_PROJECT_VERSION}")`.
- **btrbk config**: `/etc/btrbk/btrbk.conf` (canonical). `/etc/das-backup/btrbk.conf` is a symlink. `/usr/lib/das-backup/config/btrbk.conf` is a reference template only.
- **Binaries**: cmake installs to `/usr/bin/` and `/usr/libexec/`. Symlinks only if other paths need them.
- **Build artifacts**: Always build via `cmake --build build` (uses `build/cargo-target/`). Never bare `cargo build`.

## C++20 Standards
- Use C++20 features: concepts, ranges, std::format, designated initializers
- Compile with `-Wall -Wextra -Wpedantic -Werror`
- Use `std::filesystem` for all path operations

## CMake
- Minimum CMake 3.25 (for Qt6 support); host has 4.4.4
- Use ECM (Extra CMake Modules) for KDE integration
- Use `target_link_libraries` with PRIVATE/PUBLIC correctly
- Build type: RelWithDebInfo for dev, Release for install

## Qt6 / KF6
- Minimum Qt 6.6 / KF 6.0 (`gui/CMakeLists.txt`); host has Qt 6.11.2, KF 6.30.0
- Use KXmlGuiWindow for main window (KDE HIG compliance)
- Use KAboutData for application metadata
- Use KIO for file operations (restore)
- Signal/slot connections: use new-style `connect(&obj, &Class::signal, ...)`

## Rust (buttered_dasd library + btrdasd CLI)
- Rust 2024 edition, `cargo clippy` and `cargo fmt` before committing
- Library crate `buttered_dasd` exports 23 public modules (`adopt`, `backup`, `btrbk_conf`, `caldate`, `config`, `db`, `doctor`, `expire`, `forget`, `fsutil`, `health`, `indexer`, `maintenance`, `mount`, `progress`, `reconcile`, `recovery_os`, `report`, `restore`, `scanner`, `schedule`, `scrub`, `subvol`); `setup/` is binary-only. Verify with `grep -c '^pub mod ' indexer/src/lib.rs` rather than trusting this line
- Use `LazyLock<Regex>` for compile-once regex patterns (not per-call `Regex::new()`)
- Release profile: `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = true`
- All database access through `db::Database` with prepared statements
- Use `NewBackupRun` struct pattern for functions with >7 parameters
- **Mutation testing gates every push** (`.github/workflows/mutants.yml`, scored by `.github/scripts/mutants-gate.py`) plus a weekly full scope (its files are listed in the workflow). New code that spawns a command or prompts must go through a seam a test can drive (`CommandRunner`, `Prompter`, the `*_with` cores), or its mutants survive. Local run: the header of `indexer/.cargo/mutants.toml` — `--in-place`, crate-relative diff, on a copy of the tree, never `-- --lib`

## SQLite
- SQLite 3.53.2, bundled by rusqlite (libsqlite3-sys 0.38.2), with FTS5
- Use prepared statements exclusively (no string concatenation)
- WAL journal mode for concurrent read/write
- Use PRAGMA optimize on close
