# Build Conventions

## Single Canonical Source — Project-Specific Paths

See global rule in `~/.claude/rules/development-tools.md` for the full policy. Project-specific canonical locations:

- **Version source of truth**: `CMakeLists.txt` `project(VERSION ...)`. Rust gets it from `Cargo.toml`. GUI gets it via `target_compile_definitions(BTRDASD_VERSION="${CMAKE_PROJECT_VERSION}")`.
- **btrbk config**: `/etc/btrbk/btrbk.conf` (canonical). `/etc/das-backup/btrbk.conf` is a symlink. `/usr/lib/das-backup/config/btrbk.conf` is a reference template only.
- **Binaries**: cmake installs to `/usr/bin/` and `/usr/libexec/`. Symlinks only if other paths need them.
- **Shared library**: canonical at `/usr/lib/libbuttered_dasd_ffi.so`.
- **Build artifacts**: Always build via `cmake --build build` (uses `build/cargo-target/`). Never bare `cargo build`.

## C++20 Standards
- Use C++20 features: concepts, ranges, std::format, designated initializers
- Compile with `-Wall -Wextra -Wpedantic -Werror`
- Use `std::filesystem` for all path operations

## CMake
- Minimum CMake 3.25 (for Qt6 support), target 4.2.3
- Use ECM (Extra CMake Modules) for KDE integration
- Use `target_link_libraries` with PRIVATE/PUBLIC correctly
- Build type: RelWithDebInfo for dev, Release for install

## Qt6 / KF6
- Qt6 6.10.2, KDE Frameworks 6 (6.23.0)
- Use KXmlGuiWindow for main window (KDE HIG compliance)
- Use KAboutData for application metadata
- Use KIO for file operations (restore)
- Signal/slot connections: use new-style `connect(&obj, &Class::signal, ...)`

## Rust (buttered_dasd library + btrdasd CLI)
- Rust 2024 edition, `cargo clippy` and `cargo fmt` before committing
- Library crate `buttered_dasd` exports 13 public modules; `setup/` is binary-only
- Use `LazyLock<Regex>` for compile-once regex patterns (not per-call `Regex::new()`)
- Release profile: `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = true`
- All database access through `db::Database` with prepared statements
- Use `NewBackupRun` struct pattern for functions with >7 parameters

## SQLite
- SQLite 3.51.2 with FTS5 extension
- Use prepared statements exclusively (no string concatenation)
- WAL journal mode for concurrent read/write
- Use PRAGMA optimize on close
