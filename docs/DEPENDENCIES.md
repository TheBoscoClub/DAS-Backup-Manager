# DAS-Backup-Manager — Dependencies

**Version**: 0.7.22.2

## 1. Rust Crate Dependencies

These are the direct dependencies declared in `indexer/Cargo.toml` for the
`buttered_dasd` library, `btrdasd` CLI, and the `btrdasd-helper` D-Bus daemon.

### Runtime Dependencies

| Crate | Version (locked) | Purpose | License |
|-------|-----------------|---------|---------|
| `rusqlite` | 0.40.1 | SQLite bindings with FTS5 support; uses `bundled` feature to compile SQLite from source | MIT |
| `clap` | 4.6.4 | Command-line argument parsing with derive macros (`derive` feature) | MIT / Apache-2.0 |
| `walkdir` | 2.5.0 | Recursive directory traversal for snapshot indexing | Unlicense / MIT |
| `regex` | 1.13.1 | Pattern matching for snapshot path filtering | MIT / Apache-2.0 |
| `serde` | 1.0.229 | Serialization/deserialization framework with `derive` feature for TOML config | MIT / Apache-2.0 |
| `toml` | 1.1.3 | TOML parser and serializer for installer configuration | MIT / Apache-2.0 |
| `dialoguer` | 0.12.0 | Interactive terminal prompts with `fuzzy-select` feature for setup wizard | MIT |
| `console` | 0.16.4 | Terminal styling and interaction (used by dialoguer) | MIT |
| `libc` | 0.2.189 | Low-level C bindings for `geteuid()` root detection in setup module | MIT / Apache-2.0 |
| `serde_json` | 1.0.151 | JSON parsing for `lsblk --json` output in system detection and FFI interchange | MIT / Apache-2.0 |
| `clap_complete` | 4.6.8 | Shell completion generation for bash, zsh, fish, elvish, and PowerShell | MIT / Apache-2.0 |
| `tokio` | 1.53.1 | Async runtime for `btrdasd-helper` D-Bus daemon; job execution with cancellation | MIT |
| `zbus` | 5.18.0 | D-Bus implementation for `btrdasd-helper` system bus daemon | MIT |

### Dev Dependencies (test only)

| Crate | Version (locked) | Purpose | License |
|-------|-----------------|---------|---------|
| `tempfile` | 3.27.0 | Creates temporary files and directories for integration tests | MIT / Apache-2.0 |
| `filetime` | 0.2.29 | Sets file modification timestamps in tests to simulate incremental indexing | MIT / Apache-2.0 |

### Notable Transitive Dependencies

| Crate | Version | Role |
|-------|---------|------|
| `libsqlite3-sys` | 0.38.1 | Low-level SQLite FFI; compiles bundled SQLite via the `cc` crate |
| `clap_derive` | 4.6.4 | Proc-macro backend for clap derive API |
| `regex-automata` | 0.4.16 | DFA/NFA engine underlying the `regex` crate |
| `aho-corasick` | 1.1.4 | Multi-pattern string search used by `regex` |
| `toml_edit` | 0.23.10 | TOML document model underlying the `toml` crate |
| `serde_derive` | 1.0.229 | Proc-macro for `#[derive(Serialize, Deserialize)]` |

---

## 2. System Dependencies

Required on the host system for the backup scripts to operate. None of these
are automatically installed; they must be present before running the scripts.

| Tool | Version / Source | Used By | Purpose |
|------|-----------------|---------|---------|
| `btrbk` | >= 0.32 (AUR/pacman) | `backup-run.sh`, `backup-verify.sh` | BTRFS snapshot creation and send/receive to DAS targets |
| `btrfs-progs` | system (`btrfs` CLI) | `backup-run.sh`, `backup-verify.sh`, `btrdasd setup`, `btrdasd scrub`, `btrdasd doctor` | BTRFS subvolume operations, `btrfs scrub start`/`status`, subvolume listing for drift detection |
| `smartmontools` | system (`smartctl`) | `backup-run.sh`, `backup-verify.sh`, `btrdasd health` | Drive serial number detection, SMART health, temperature, power-on hours |
| `rsync` | system | Manual disaster-recovery restores only (see `docs/DISASTER-RECOVERY-GUIDE.md`) | Not used by any automated script — ESP synchronization was removed 2026-04-10/12 (`.claude/rules/esp-safety.md`) |
| `s-nail` (mailx) | system | `backup-run.sh`, `indexer/src/report.rs` | Submits email backup reports to the local mail relay; invoked as `mailx` |
| a local mail relay | system (optional) | `backup-run.sh`, `indexer/src/report.rs` | Anything accepting SMTP on `[email].smtp_host:smtp_port` — a Postfix null-client smarthost here. Required only when `[email].enabled = true`; without it reports are written to disk and the send is logged as failed |
| `mount` / `umount` | system (util-linux) | `backup-run.sh`, `backup-verify.sh` | Mounts BTRFS source volumes and DAS targets before backup |
| `df` | system (coreutils) | `backup-run.sh` | Disk space reporting and throughput calculation |
| `date` | system (coreutils) | All scripts | Timestamp generation and ISO 8601 epoch arithmetic |
| `awk` | system (gawk) | All scripts | Parsing smartctl and df output |
| `bash` | >= 4.0 | All scripts | Runtime shell; scripts use `#!/bin/bash` with `set -euo pipefail` |
| `parted` / `mkfs.btrfs` / `mkfs.fat` | system | `das-partition-drives.sh` | Initial drive partitioning and formatting (one-time setup only) |
| `mbuffer` | system (optional) | `btrbk` (via config) | Buffered stream transfers with progress; btrbk uses it if configured |
| `lsblk` | system (util-linux) | `btrdasd setup` | Block device detection via JSON output |

### Init System Support

The installer supports three init systems. Only one is required:

| Init System | Service/Timer | Detection |
|-------------|---------------|-----------|
| **systemd** | `.service` + `.timer` units | `/sbin/init` symlink to systemd |
| **sysvinit** | cron entries | `/sbin/init` without systemd |
| **OpenRC** | cron entries | `/sbin/openrc-init` exists |

### SMTP Configuration

`backup-run.sh` (`send_report`) and the Rust reporter
(`indexer/src/report.rs::send_email_report_with_kind`) both submit through
`s-nail` (mailx) to the relay named by `[email].smtp_host`/`smtp_port` in
`/etc/das-backup/config.toml` — `127.0.0.1:25` by default.

**This project holds no mail credential.** Submission is unauthenticated
plaintext over loopback; the relay authenticates to the upstream provider
itself, selecting a key by envelope sender (`[email].from`), and owns the
certificate-verified TLS leg. `-S smtp-auth=none` is required on every mailx
invocation: s-nail otherwise demands a password for any `smtp://` mta and
exits 4.

Migrated from Protonmail Bridge (`127.0.0.1:1025`, STARTTLS, self-signed cert,
`ssl-verify=ignore`) on 2026-08-06 — Bridge's vault decrypts through the
desktop keyring, so it is dead after an unattended reboot and every report was
silently lost until someone logged in.

---

## 3. Build Dependencies

Required to compile the `btrdasd` indexer binary from source.

| Tool | Version | Purpose |
|------|---------|---------|
| Rust toolchain | **1.87 or later** | Cargo.toml specifies `edition = "2024"`. The `let_chains` feature used by the setup module requires Rust 1.87+. Tested with 1.93.1. |
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
minimum required version is Rust 1.87 due to `let_chains` in edition 2024.

---

## 4. GUI Dependencies (Optional)

The KDE Plasma GUI (`btrdasd-gui`) requires the following. These are only
needed when building with `BUILD_GUI=ON` (the default).

| Dependency | Version Target | Purpose | License |
|-----------|---------------|---------|---------|
| Qt6 | 6.6+ (tested 6.10.2) | UI framework: Core, Widgets, DBus, Charts (growth trendline) | LGPL-3.0 |
| KDE Frameworks 6 (KF6) | 6.0+ (tested 6.23.0) | KXmlGuiWindow, KAboutData, KIO for restore operations, KDE HIG compliance | LGPL-2.1 / LGPL-3.0 |
| CMake | >= 3.25 (tested 4.2.3) | Build system for the Qt/KF6 C++20 GUI component | BSD-3-Clause |
| Extra CMake Modules (ECM) | ships with KF6 | KDE-specific CMake macros and platform integration | BSD-2-Clause |

### KF6 Modules Used

| Module | Purpose |
|--------|---------|
| CoreAddons | KAboutData, application metadata |
| I18n | KLocalizedString for translations |
| XmlGui | KXmlGuiWindow, KStandardAction, toolbar XML |
| ConfigWidgets | KConfigDialog for settings |
| IconThemes | KDE icon theme integration |
| Crash | KCrash for crash reporting |
| KIO | KIO::copy for file restore operations |
| Notifications | KNotification for backup complete/fail desktop notifications |
| StatusNotifierItem | KStatusNotifierItem for system tray integration |

The GUI links against `Qt6::DBus` for all communication with `btrdasd-helper` — every read
and write goes through D-Bus. There is no `Qt6::Sql` dependency and no direct database
connection: the previous read-only `QSqlDatabase` wrapper (`database.h/cpp`) was removed
when the GUI's models were rewired onto `DBusClient` exclusively. `Qt6::Test` is still
found under `BUILD_TESTING` for `ECMAddTests`, but no test target currently uses it (see
the Tests section of `docs/ARCHITECTURE.md`).

