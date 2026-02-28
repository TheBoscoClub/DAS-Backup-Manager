# Full Management Interface Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Transform DAS-Backup-Manager from a read-only browser into a full backup management system with CLI-GUI parity, D-Bus privilege escalation, comprehensive documentation, and a Dolphin-style file browser.

**Architecture:** Rust library (libbuttered_dasd) as single source of truth for all business logic. C-ABI FFI layer for GUI consumption. D-Bus activated helper daemon (btrdasd-helper) with polkit authorization for privileged operations. CLI and GUI both consume the library. See `docs/plans/2026-02-24-full-management-interface-design.md` for full architecture.

**Tech Stack:** Rust 2024 edition, rusqlite 0.38, clap 4.5, zbus (D-Bus), C++20, Qt6 6.10.2, KF6 6.23.0, CMake 4.2.3

**Design Doc:** `docs/plans/2026-02-24-full-management-interface-design.md`

---

## Phase 1: Rust Library Refactoring (Foundation)

Everything else depends on the library. This phase restructures the Rust crate from a CLI-only binary into a library + binary architecture, adds new modules, and changes the config data model.

### Task 1.1: Split Crate into Library + Binary

**Files:**
- Modify: `indexer/Cargo.toml`
- Modify: `indexer/src/lib.rs`
- Modify: `indexer/src/main.rs`
- Move: `indexer/src/setup/config.rs` → accessible via `lib.rs`

**Step 1:** Update `Cargo.toml` to produce both a library and a binary:

```toml
[lib]
name = "buttered_dasd"
path = "src/lib.rs"

[[bin]]
name = "btrdasd"
path = "src/main.rs"
```

**Step 2:** Update `lib.rs` to re-export `setup::config` as a public module:

```rust
pub mod db;
pub mod indexer;
pub mod scanner;
pub mod config {
    pub use crate::setup::config::*;
}
// setup module stays for wizard/installer (binary-only concerns)
mod setup;
```

Wait — `setup` is currently only in `main.rs` scope. Need to make the config types accessible from the library while keeping wizard/installer as binary-only.

**Step 3:** Restructure modules:
- Move `setup/config.rs` content into a new top-level `src/config.rs` (library-public)
- Update `setup/` modules to import from the new location
- Update `main.rs` to import from the library

**Step 4:** Run `cargo test` — all existing tests must pass.

**Step 5:** Commit: `refactor: split crate into library + binary`

### Task 1.2: SubvolConfig Data Model Migration

**Files:**
- Modify: `indexer/src/config.rs` (new location)
- Modify: `indexer/src/setup/wizard.rs`
- Modify: `indexer/src/setup/templates.rs`
- Modify: `indexer/src/setup/env_export.rs`
- Test: existing config tests + new migration tests

**Step 1:** Write failing tests for the new SubvolConfig type:

```rust
#[test]
fn subvol_config_from_string() {
    let toml = r#"
    [[source]]
    label = "test"
    volume = "/vol"
    device = "/dev/sda"
    subvolumes = ["@", "@home"]
    "#;
    let cfg: Config = Config::from_toml(toml).unwrap();
    assert_eq!(cfg.sources[0].subvolumes[0].name, "@");
    assert!(!cfg.sources[0].subvolumes[0].manual_only);
}

#[test]
fn subvol_config_full_format() {
    let toml = r#"
    [[source]]
    label = "test"
    volume = "/vol"
    device = "/dev/sda"
    [[source.subvolumes]]
    name = "@"
    [[source.subvolumes]]
    name = "@root"
    manual_only = true
    "#;
    let cfg: Config = Config::from_toml(toml).unwrap();
    assert_eq!(cfg.sources[0].subvolumes[1].name, "@root");
    assert!(cfg.sources[0].subvolumes[1].manual_only);
}
```

**Step 2:** Run tests — verify they fail.

**Step 3:** Implement `SubvolConfig` and custom serde deserializer:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct SubvolConfig {
    pub name: String,
    #[serde(default)]
    pub manual_only: bool,
}

// Custom deserializer: accepts both bare strings and full structs
impl<'de> Deserialize<'de> for SubvolConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum SubvolEntry {
            Simple(String),
            Full { name: String, #[serde(default)] manual_only: bool },
        }
        match SubvolEntry::deserialize(deserializer)? {
            SubvolEntry::Simple(name) => Ok(SubvolConfig { name, manual_only: false }),
            SubvolEntry::Full { name, manual_only } => Ok(SubvolConfig { name, manual_only }),
        }
    }
}
```

**Step 4:** Change `Source.subvolumes` from `Vec<String>` to `Vec<SubvolConfig>`.

**Step 5:** Update all code that accesses `source.subvolumes` — wizard.rs, templates.rs, env_export.rs. Anywhere that iterates `subvolumes` as strings now needs `.name`.

**Step 6:** Run `cargo test` — all tests pass including new ones.

**Step 7:** Commit: `feat: add SubvolConfig with manual_only flag and backward-compat deserialization`

### Task 1.3: Progress Callback Trait

**Files:**
- Create: `indexer/src/progress.rs`
- Test: inline unit tests

**Step 1:** Write the progress module:

```rust
/// Log level for progress messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

/// Callback trait for reporting progress from long-running operations.
/// Implementations must be Send + Sync for use across threads.
pub trait ProgressCallback: Send + Sync {
    fn on_stage(&self, stage: &str, total_steps: u64);
    fn on_progress(&self, current: u64, total: u64, message: &str);
    fn on_throughput(&self, bytes_per_sec: u64);
    fn on_log(&self, level: LogLevel, message: &str);
    fn on_complete(&self, success: bool, summary: &str);
}

/// No-op implementation for when progress reporting isn't needed.
pub struct NullProgress;

impl ProgressCallback for NullProgress {
    fn on_stage(&self, _: &str, _: u64) {}
    fn on_progress(&self, _: u64, _: u64, _: &str) {}
    fn on_throughput(&self, _: u64) {}
    fn on_log(&self, _: LogLevel, _: &str) {}
    fn on_complete(&self, _: bool, _: &str) {}
}

/// Collects progress events into vectors for testing.
#[cfg(test)]
pub struct TestProgress {
    pub stages: std::sync::Mutex<Vec<(String, u64)>>,
    pub logs: std::sync::Mutex<Vec<(LogLevel, String)>>,
}
```

**Step 2:** Add `pub mod progress;` to `lib.rs`.

**Step 3:** Run `cargo test`.

**Step 4:** Commit: `feat: add progress callback trait for long-running operations`

### Task 1.4: Backup Module

**Files:**
- Create: `indexer/src/backup.rs`
- Test: inline unit tests (mock btrbk calls)

**Step 1:** Write the backup orchestration module. This wraps btrbk CLI calls and script execution:

```rust
use crate::config::{Config, TargetRole};
use crate::progress::{LogLevel, NullProgress, ProgressCallback};
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum BackupMode {
    Incremental,
    Full,
}

#[derive(Debug, Default)]
pub struct BackupOptions {
    pub mode: Option<BackupMode>,
    pub sources: Vec<String>,      // empty = all
    pub targets: Vec<String>,      // empty = all available
    pub dry_run: bool,
    pub snapshot_only: bool,       // create snapshots, skip send
    pub send_only: bool,           // send existing, skip snapshot
    pub boot_archive: bool,        // include boot archival
    pub index_after: bool,         // run indexer after backup
    pub send_report: bool,         // email report after
}

#[derive(Debug)]
pub struct BackupResult {
    pub success: bool,
    pub snapshots_created: usize,
    pub snapshots_sent: usize,
    pub bytes_sent: u64,
    pub boot_archived: bool,
    pub indexed: bool,
    pub report_sent: bool,
    pub errors: Vec<String>,
    pub duration_secs: u64,
}

/// Run a backup with the given options. Calls btrbk under the hood.
/// The caller must ensure this runs with appropriate privileges (root).
pub fn run_backup(
    config: &Config,
    options: &BackupOptions,
    progress: &dyn ProgressCallback,
) -> Result<BackupResult, Box<dyn std::error::Error>> {
    // Implementation: orchestrate btrbk snapshot, send/receive, boot archive,
    // indexing, and email report based on options.
    // Each stage reports progress via the callback.
    todo!("Implement in Phase 1.4")
}

/// Create btrbk snapshots for specified sources.
pub fn create_snapshots(
    config: &Config,
    sources: &[String],
    progress: &dyn ProgressCallback,
) -> Result<usize, Box<dyn std::error::Error>> {
    todo!()
}

/// Send snapshots to specified targets via btrbk.
pub fn send_snapshots(
    config: &Config,
    sources: &[String],
    targets: &[String],
    progress: &dyn ProgressCallback,
) -> Result<(usize, u64), Box<dyn std::error::Error>> {
    todo!()
}

/// Archive boot subvolumes as read-only snapshots.
pub fn archive_boot(
    config: &Config,
    progress: &dyn ProgressCallback,
) -> Result<bool, Box<dyn std::error::Error>> {
    todo!()
}
```

**Step 2:** Add `pub mod backup;` to `lib.rs`.

**Step 3:** Write tests for the pure logic parts (option filtering, source/target matching). The actual btrbk calls will be integration-tested on VMs.

**Step 4:** Run `cargo test`.

**Step 5:** Commit: `feat: add backup orchestration module (skeleton)`

### Task 1.5: Restore Module

**Files:**
- Create: `indexer/src/restore.rs`
- Test: inline unit tests

**Step 1:** Write the restore module:

```rust
use crate::progress::ProgressCallback;
use std::path::Path;

/// Restore specific files from a snapshot to a destination.
pub fn restore_files(
    snapshot_path: &Path,
    file_paths: &[&str],
    dest: &Path,
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    todo!()
}

/// Restore an entire snapshot to a destination directory.
pub fn restore_snapshot(
    snapshot_path: &Path,
    dest: &Path,
    progress: &dyn ProgressCallback,
) -> Result<RestoreResult, Box<dyn std::error::Error>> {
    todo!()
}

/// Browse files in a snapshot directory, returning entries matching an optional path prefix.
pub fn browse_snapshot(
    snapshot_path: &Path,
    prefix: Option<&str>,
) -> Result<Vec<BrowseEntry>, Box<dyn std::error::Error>> {
    todo!()
}
```

**Step 2:** Add `pub mod restore;` to `lib.rs`.

**Step 3:** Run tests, commit: `feat: add restore module (skeleton)`

### Task 1.6: Schedule Module

**Files:**
- Create: `indexer/src/schedule.rs`
- Test: inline unit tests

```rust
use crate::config::{Config, InitSystem};

pub struct ScheduleInfo {
    pub incremental_time: String,
    pub full_schedule: String,
    pub delay_min: u32,
    pub enabled: bool,
    pub next_incremental: Option<String>,
    pub next_full: Option<String>,
}

/// Get current schedule info by querying systemd timers (or cron).
pub fn get_schedule(config: &Config) -> Result<ScheduleInfo, Box<dyn std::error::Error>> {
    todo!()
}

/// Modify the backup schedule. Updates config AND regenerates timer/cron files.
pub fn set_schedule(
    config: &mut Config,
    incremental: Option<&str>,
    full: Option<&str>,
    delay: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    todo!()
}

/// Enable or disable scheduled backups.
pub fn set_enabled(config: &Config, enabled: bool) -> Result<(), Box<dyn std::error::Error>> {
    todo!()
}
```

**Step 2:** Add `pub mod schedule;` to `lib.rs`.

**Step 3:** Tests, commit: `feat: add schedule management module (skeleton)`

### Task 1.7: Subvolume Management Module

**Files:**
- Create: `indexer/src/subvol.rs`
- Test: inline unit tests

```rust
use crate::config::{Config, SubvolConfig};

/// List all subvolumes across all sources with their schedule status.
pub fn list_subvolumes(config: &Config) -> Vec<SubvolInfo> {
    config.sources.iter().flat_map(|src| {
        src.subvolumes.iter().map(|sv| SubvolInfo {
            source_label: src.label.clone(),
            name: sv.name.clone(),
            manual_only: sv.manual_only,
        })
    }).collect()
}

/// Add a subvolume to a source.
pub fn add_subvolume(
    config: &mut Config,
    source_label: &str,
    name: &str,
    manual_only: bool,
) -> Result<(), String> {
    let src = config.sources.iter_mut()
        .find(|s| s.label == source_label)
        .ok_or_else(|| format!("Source '{}' not found", source_label))?;
    if src.subvolumes.iter().any(|sv| sv.name == name) {
        return Err(format!("Subvolume '{}' already exists in source '{}'", name, source_label));
    }
    src.subvolumes.push(SubvolConfig { name: name.to_string(), manual_only });
    Ok(())
}

/// Remove a subvolume from a source.
pub fn remove_subvolume(config: &mut Config, source_label: &str, name: &str) -> Result<(), String> {
    let src = config.sources.iter_mut()
        .find(|s| s.label == source_label)
        .ok_or_else(|| format!("Source '{}' not found", source_label))?;
    let len_before = src.subvolumes.len();
    src.subvolumes.retain(|sv| sv.name != name);
    if src.subvolumes.len() == len_before {
        return Err(format!("Subvolume '{}' not found in source '{}'", name, source_label));
    }
    Ok(())
}

/// Set a subvolume's manual_only flag.
pub fn set_manual(config: &mut Config, source_label: &str, name: &str, manual: bool) -> Result<(), String> {
    let src = config.sources.iter_mut()
        .find(|s| s.label == source_label)
        .ok_or_else(|| format!("Source '{}' not found", source_label))?;
    let sv = src.subvolumes.iter_mut()
        .find(|sv| sv.name == name)
        .ok_or_else(|| format!("Subvolume '{}' not found", name))?;
    sv.manual_only = manual;
    Ok(())
}
```

**Step 2:** Tests for add/remove/set_manual, commit: `feat: add subvolume management module`

### Task 1.8: Health Module

**Files:**
- Create: `indexer/src/health.rs`
- Test: inline tests for parsing functions

```rust
/// Query SMART data, disk space, target availability, growth trends.
/// Parsing functions are pure and testable. System calls are integration-tested.
```

Commit: `feat: add health monitoring module (skeleton)`

### Task 1.9: Report Module

**Files:**
- Create: `indexer/src/report.rs`

Backup history recording (new DB tables), email report generation.

Commit: `feat: add report module with backup history DB tables`

### Task 1.10: New DB Tables

**Files:**
- Modify: `indexer/src/db.rs`

Add `backup_runs` and `target_usage` tables to the schema. Add methods:
- `insert_backup_run()`
- `get_backup_history(limit)`
- `insert_target_usage()`
- `get_target_usage_history(target_label, days)`

Commit: `feat: add backup_runs and target_usage DB tables`

---

## Phase 2: CLI Expansion

Add all new subcommands to `btrdasd`. Each subcommand consumes the library.

### Task 2.1: Add `backup` Subcommand Group

**Files:**
- Modify: `indexer/src/main.rs`

Add `backup run`, `backup snapshot`, `backup send`, `backup boot-archive`, `backup report` subcommands with full clap definitions including `long_about` and examples.

Commit: `feat: add backup subcommand group to CLI`

### Task 2.2: Add `restore` Subcommand Group

Add `restore file`, `restore snapshot`, `restore browse`.

Commit: `feat: add restore subcommand group to CLI`

### Task 2.3: Add `schedule` Subcommand Group

Add `schedule show`, `schedule set`, `schedule enable`, `schedule disable`, `schedule next`.

Commit: `feat: add schedule subcommand group to CLI`

### Task 2.4: Add `subvol` Subcommand Group

Add `subvol list`, `subvol add`, `subvol remove`, `subvol set-manual`, `subvol set-auto`.

Commit: `feat: add subvol management subcommand group to CLI`

### Task 2.5: Add `health` Subcommand

Commit: `feat: add health monitoring subcommand to CLI`

### Task 2.6: Add `config edit` Subcommand

Dot-notation config editing.

Commit: `feat: add config edit subcommand for programmatic config changes`

### Task 2.7: Add `--json` Global Flag

Add JSON output support to all read commands.

Commit: `feat: add --json flag for machine-readable output`

### Task 2.8: Enhanced --help Text

Add `long_about`, `after_help` with examples to every subcommand.

Commit: `docs: add comprehensive --help text with examples to all CLI subcommands`

### Task 2.9: Shell Completions

Add clap `generate` for bash, zsh, fish completions. Add CMake install targets.

Commit: `feat: add shell completions for bash, zsh, fish`

---

## Phase 3: D-Bus Helper + Polkit

### Task 3.1: Add zbus Dependency and Helper Binary

**Files:**
- Modify: `indexer/Cargo.toml` (add zbus, tokio)
- Create: `indexer/src/bin/btrdasd-helper.rs`

New binary target in Cargo.toml:

```toml
[[bin]]
name = "btrdasd-helper"
path = "src/bin/btrdasd-helper.rs"
```

Commit: `feat: add btrdasd-helper D-Bus daemon skeleton`

### Task 3.2: D-Bus Interface Implementation

Implement all D-Bus methods from the design doc (BackupRun, RestoreFiles, ConfigGet/Set, ScheduleGet/Set, SubvolAdd/Remove, HealthQuery, etc.) plus progress signals.

Commit: `feat: implement D-Bus interface for privileged operations`

### Task 3.3: Polkit Policy

**Files:**
- Create: `polkit/org.dasbackup.policy`
- Create: `dbus/org.dasbackup.Helper1.service`
- Create: `dbus/org.dasbackup.Helper1.conf`

Commit: `feat: add polkit policy and D-Bus service activation files`

### Task 3.4: CMake Integration for Helper

Update top-level CMakeLists.txt to build and install the helper, polkit, and D-Bus files.

Commit: `build: add btrdasd-helper to CMake build and install targets`

---

## Phase 4: GUI Expansion

### Task 4.1: FFI Layer

**Files:**
- Create: `indexer/src/ffi.rs`
- Modify: `indexer/Cargo.toml` (add cdylib target)

C-ABI wrapper functions that the GUI can link against. Exposes config read, subvol list, health query, and progress callback registration.

Commit: `feat: add C-ABI FFI layer for GUI consumption`

### Task 4.2: D-Bus Client Class (C++)

**Files:**
- Create: `gui/src/dbusclient.h`
- Create: `gui/src/dbusclient.cpp`

Qt D-Bus wrapper class for calling btrdasd-helper methods and receiving progress signals. Uses `QDBusInterface` and `QDBusConnection`.

Commit: `feat: add D-Bus client class for GUI privilege escalation`

### Task 4.3: Navigation Sidebar

**Files:**
- Create: `gui/src/sidebar.h`
- Create: `gui/src/sidebar.cpp`
- Modify: `gui/src/mainwindow.h`
- Modify: `gui/src/mainwindow.cpp`

Replace the current layout with navigation sidebar + central area architecture.

Commit: `feat: add navigation sidebar to main window`

### Task 4.4: File Browser Widget

**Files:**
- Create: `gui/src/snapshotbrowser.h`
- Create: `gui/src/snapshotbrowser.cpp`

Dolphin-style file browser with KUrlNavigator breadcrumbs, detail/icon/tree views, multi-select, context menu for restore.

Commit: `feat: add Dolphin-style snapshot file browser widget`

### Task 4.5: Backup Operations Panel

**Files:**
- Create: `gui/src/backuppanel.h`
- Create: `gui/src/backuppanel.cpp`

"Run Now" panel with mode selector, operation checkboxes, source/target checkboxes, dry-run button.

Commit: `feat: add backup operations panel with manual run support`

### Task 4.6: Config Editor Dialog

**Files:**
- Create: `gui/src/configdialog.h`
- Create: `gui/src/configdialog.cpp`

KPageDialog with tabs for Sources, Targets, Schedule, ESP, Email, Boot, DAS, General. Replace minimal SettingsDialog.

Commit: `feat: add comprehensive config editor dialog`

### Task 4.7: First-Run Wizard

**Files:**
- Create: `gui/src/setupwizard.h`
- Create: `gui/src/setupwizard.cpp`

KAssistantDialog with 10 wizard pages matching CLI wizard flow.

Commit: `feat: add first-run setup wizard dialog`

### Task 4.8: Progress Panel

**Files:**
- Create: `gui/src/progresspanel.h`
- Create: `gui/src/progresspanel.cpp`

QDockWidget with structured progress bars, ETA, throughput, collapsible raw log.

Commit: `feat: add progress panel with structured progress and raw log`

### Task 4.9: Health Dashboard

**Files:**
- Create: `gui/src/healthdashboard.h`
- Create: `gui/src/healthdashboard.cpp`

Drive SMART status table, growth chart (QChart), system status overview.

Commit: `feat: add health monitoring dashboard`

### Task 4.10: Backup History View

**Files:**
- Create: `gui/src/backuphistory.h`
- Create: `gui/src/backuphistory.cpp`

Table view of backup_runs with timestamp, mode, duration, result, bytes sent.

Commit: `feat: add backup history table view`

### Task 4.11: Desktop Integration

KNotification for backup events, optional KStatusNotifierItem tray icon, keyboard shortcuts.

Commit: `feat: add KDE desktop integration (notifications, tray, shortcuts)`

### Task 4.12: Update CMakeLists.txt

Add all new GUI source files, Qt6::Charts dependency, Qt6::DBus dependency.

Commit: `build: update GUI CMakeLists for new components`

---

## Phase 5: Documentation

### Task 5.1: Man Page

**Files:**
- Create: `docs/btrdasd.1`

Full roff man page covering all subcommands, config format, files, environment, examples.

Commit: `docs: add comprehensive man page (btrdasd.1)`

### Task 5.2: Info Page

**Files:**
- Create: `docs/btrdasd.texi`

GNU Texinfo tutorial-style documentation.

Commit: `docs: add GNU info page`

### Task 5.3: HTML Documentation

**Files:**
- Create: `docs/html/index.html` (generated from man page or standalone)

Commit: `docs: add HTML documentation`

### Task 5.4: Install Targets for Docs

Update CMakeLists.txt to install man page, info page, HTML docs to standard locations.

Commit: `build: add install targets for documentation`

### Task 5.5: Update README and CHANGELOG

Update README.md with new features, update CHANGELOG.md with 0.6.0 entry, update ARCHITECTURE.md with new components.

Commit: `docs: update README, CHANGELOG, and ARCHITECTURE for v0.6.0`

### Task 5.6: Version Bump

Bump version in Cargo.toml, CMakeLists.txt, and config defaults from 0.5.1 to 0.6.0.

Commit: `chore: bump version to 0.6.0`

---

## Implementation Order and Dependencies

```
Phase 1 (Rust Library) ──→ Phase 2 (CLI) ──→ Phase 5 (Docs)
         │                        │
         └── Phase 3 (D-Bus) ─────┴──→ Phase 4 (GUI)
```

- Phase 1 blocks everything
- Phase 2 and Phase 3 can run in parallel after Phase 1
- Phase 4 requires both Phase 1 (FFI) and Phase 3 (D-Bus)
- Phase 5 can start after Phase 2 (CLI subcommands defined)

## Estimated Task Count

- Phase 1: 10 tasks
- Phase 2: 9 tasks
- Phase 3: 4 tasks
- Phase 4: 12 tasks
- Phase 5: 6 tasks
- **Total: 41 tasks**
