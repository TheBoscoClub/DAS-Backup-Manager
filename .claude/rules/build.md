# Build Conventions

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
