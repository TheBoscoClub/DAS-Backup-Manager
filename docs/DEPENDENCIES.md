# DAS-Backup-Manager — Dependencies

## 1. Rust Crate Dependencies

These are the direct dependencies declared in `indexer/Cargo.toml` for the
`buttered-dasd` crate (binary: `btrdasd`).

### Runtime Dependencies

| Crate | Version (locked) | Purpose | License |
|-------|-----------------|---------|---------|
| `rusqlite` | 0.38.0 | SQLite bindings with FTS5 support; uses `bundled` feature to compile SQLite 3.47.x from source — no system SQLite required | MIT |
| `clap` | 4.5.60 | Command-line argument parsing with derive macros (`derive` feature) | MIT / Apache-2.0 |
| `walkdir` | 2.5.0 | Recursive directory traversal for snapshot indexing | Unlicense / MIT |
| `regex` | 1.12.3 | Pattern matching for snapshot path filtering | MIT / Apache-2.0 |

### Dev Dependencies (test only)

| Crate | Version (locked) | Purpose | License |
|-------|-----------------|---------|---------|
| `tempfile` | 3.25.0 | Creates temporary files and directories for integration tests | MIT / Apache-2.0 |
| `filetime` | 0.2.27 | Sets file modification timestamps in tests to simulate incremental indexing | MIT / Apache-2.0 |

### Notable Transitive Dependencies

| Crate | Version | Role |
|-------|---------|------|
| `libsqlite3-sys` | 0.36.0 | Low-level SQLite FFI; compiles bundled SQLite via the `cc` crate |
| `clap_derive` | 4.5.55 | Proc-macro backend for clap derive API |
| `regex-automata` | 0.4.14 | DFA/NFA engine underlying the `regex` crate |
| `aho-corasick` | 1.1.4 | Multi-pattern string search used by `regex` |

---

## 2. System Dependencies

Required on the host system for the backup scripts to operate. None of these
are automatically installed; they must be present before running the scripts.

| Tool | Version / Source | Used By | Purpose |
|------|-----------------|---------|---------|
| `btrbk` | ≥ 0.32 (AUR/pacman) | `backup-run.sh`, `backup-verify.sh` | BTRFS snapshot creation and send/receive to DAS targets |
| `btrfs-progs` | system (`btrfs` CLI) | `backup-run.sh`, `backup-verify.sh` | BTRFS subvolume operations: list, snapshot, delete, usage, label |
| `smartmontools` | system (`smartctl`) | `backup-run.sh`, `backup-verify.sh` | Drive serial number detection, SMART health, temperature, power-on hours |
| `rsync` | system | `backup-run.sh` | ESP synchronization from `/boot` to DAS bootable recovery drives |
| `s-nail` (mailx) | system | `backup-run.sh` | Sends email backup reports via SMTP (Proton Bridge); invoked as `mailx` |
| `msmtp` | system (optional) | `backup-run.sh` | Alternative SMTP transport; `s-nail` is the primary sender |
| `systemd` | system | `install-backup-timer.sh` | Schedules nightly backup via `das-backup.timer` / `das-backup.service` |
| `mount` / `umount` | system (util-linux) | `backup-run.sh`, `backup-verify.sh` | Mounts BTRFS source volumes and DAS targets before backup |
| `df` | system (coreutils) | `backup-run.sh` | Disk space reporting and throughput calculation |
| `date` | system (coreutils) | All scripts | Timestamp generation and ISO 8601 epoch arithmetic |
| `awk` | system (gawk) | All scripts | Parsing smartctl and df output |
| `zsh` | ≥ 5.9 | All scripts | Runtime shell; scripts use `#!/usr/bin/env zsh` and zsh-specific features (`zsh/datetime`, `declare -A`, `(N)` glob qualifier) |
| `parted` / `mkfs.btrfs` / `mkfs.fat` | system | `das-partition-drives.sh` | Initial drive partitioning and formatting (one-time setup only) |

### SMTP Configuration

`backup-run.sh` reads `/etc/das-backup-email.conf` (mode 600) for SMTP
credentials. The mailer is `s-nail` (mailx). Proton Bridge is the configured
SMTP relay; `msmtp` may substitute if preferred.

---

## 3. Build Dependencies

Required to compile the `btrdasd` indexer binary from source.

| Tool | Version | Purpose |
|------|---------|---------|
| Rust toolchain | **1.85 or later** | Cargo.toml specifies `edition = "2024"`, which requires Rust 1.85+. Tested with 1.93.1 (Arch Linux, as of 2026-02). |
| `cargo` | ships with Rust | Package manager and build system |
| `cc` (C compiler, gcc/clang) | system | Required by `libsqlite3-sys` to compile bundled SQLite from C source |
| `pkg-config` | system | Used by `libsqlite3-sys` to locate system SQLite if the `bundled` feature is removed |

### Build Command

```bash
cargo build --release --manifest-path indexer/Cargo.toml
# Output: indexer/target/release/btrdasd
# Install to: /usr/local/bin/btrdasd
```

No `rust-toolchain.toml` is present; the stable channel is assumed. The
minimum required version is Rust 1.85 due to the 2024 edition.

---

## 4. Optional Dependencies (Future GUI)

The planned KDE Plasma GUI layer (not yet implemented) will require the
following when developed.

| Dependency | Version Target | Purpose | License |
|-----------|---------------|---------|---------|
| Qt6 | 6.10.2 | UI framework: widgets, signals/slots, model/view | LGPL-3.0 |
| KDE Frameworks 6 (KF6) | 6.23.0 | KXmlGuiWindow, KAboutData, KIO for restore operations, KDE HIG compliance | LGPL-2.1 / LGPL-3.0 |
| CMake | ≥ 4.2.3 (min 3.25) | Build system for the Qt/KF6 C++20 GUI component | BSD-3-Clause |
| Extra CMake Modules (ECM) | ships with KF6 | KDE-specific CMake macros and platform integration | BSD-2-Clause |
| SQLite 3.51.2 | system or bundled | Shared database access between GUI and indexer (FTS5 search) | Public Domain |

The GUI will be written in C++20 and link against the same SQLite database
written by `btrdasd`. See `CLAUDE.md` and `.claude/rules/build.md` for the
full C++20/Qt6/KF6 conventions.
