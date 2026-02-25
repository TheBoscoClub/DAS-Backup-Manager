# Full Management Interface Design

**Date**: 2026-02-24
**Version**: 0.6.0 (target)
**Scope**: Transform DAS-Backup-Manager from a read-only browser into a full backup management system with CLI parity in the GUI.

## 1. Architecture

### Component Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    User Space                               │
│                                                             │
│  ┌──────────┐    ┌──────────────────────────────────────┐   │
│  │ btrdasd  │    │         btrdasd-gui (Qt6/KF6)        │   │
│  │  (CLI)   │    │                                      │   │
│  │          │    │  File Browser │ Config │ Monitor      │   │
│  └────┬─────┘    └──────────┬───────────────────────────┘   │
│       │                     │                               │
│       │    ┌────────────────┴──────────────┐                │
│       │    │  libbuttered_dasd_ffi (C ABI) │                │
│       │    └────────────────┬──────────────┘                │
│       │                     │                               │
│  ┌────┴─────────────────────┴──────────────────────┐        │
│  │          libbuttered_dasd (Rust library)         │        │
│  │                                                  │        │
│  │  indexer │ config │ backup │ restore │ schedule   │        │
│  │  search  │ health │ subvol │ progress│ reporting  │        │
│  └──────────────────────┬───────────────────────────┘        │
│                         │                                    │
│                         │ D-Bus (org.dasbackup.Helper1)      │
│                         │                                    │
│  ┌──────────────────────┴────────────────────────┐           │
│  │  btrdasd-helper (privileged daemon)           │           │
│  │  polkit-authorized, runs as root              │           │
│  │                                               │           │
│  │  btrbk │ mount │ DB write │ config write      │           │
│  │  SMART │ systemd-timer │ btrfs commands       │           │
│  └───────────────────────────────────────────────┘           │
│                                                              │
├──────────────────────────────────────────────────────────────┤
│                    Kernel / Filesystem                        │
│  BTRFS snapshots │ SQLite DB │ /etc/das-backup/config.toml   │
└──────────────────────────────────────────────────────────────┘
```

### Five Binaries

| Binary | Language | Runs as | Purpose |
|--------|----------|---------|---------|
| `libbuttered_dasd` | Rust (rlib/staticlib) | library | All business logic — single source of truth |
| `libbuttered_dasd_ffi` | Rust (cdylib) | shared library | Thin C-ABI wrapper for GUI consumption |
| `btrdasd` | Rust | user/sudo | CLI frontend consuming the library directly |
| `btrdasd-helper` | Rust | root (D-Bus activated) | Privileged operations, polkit-gated |
| `btrdasd-gui` | C++20/Qt6/KF6 | user | GUI frontend, links FFI, talks to helper via D-Bus |

### Privilege Split

**Unprivileged** (user-space, no escalation needed):
- `search`, `list`, `info` — read-only DB queries
- `config show`, `config validate` — read config file
- Browse snapshots (read filesystem)
- Read stats, growth log

**Privileged** (via D-Bus helper with polkit authorization):
- `walk` / index — writes to root-owned DB
- `backup run` — calls btrbk (requires root for btrfs send/receive)
- `backup snapshot` — creates btrfs snapshots (root)
- `backup send` — btrfs send/receive to targets (root)
- `backup boot-archive` — snapshots boot subvolumes (root)
- `restore` — writes to arbitrary paths (needs permission)
- `config write` — writes to /etc/das-backup/config.toml (root)
- `schedule set/enable/disable` — modifies systemd timers (root)
- `subvol add/remove/set-manual/set-auto` — modifies config (root)
- `mount/unmount` — target filesystem management (root)
- SMART queries — `smartctl` requires root/disk group

### D-Bus Helper Design

**Bus name**: `org.dasbackup.Helper1`
**Object path**: `/org/dasbackup/Helper1`
**Interface**: `org.dasbackup.Helper1`

**Polkit actions** (in `/usr/share/polkit-1/actions/org.dasbackup.policy`):
- `org.dasbackup.backup` — run backups (auth_admin_keep)
- `org.dasbackup.restore` — restore files (auth_admin_keep)
- `org.dasbackup.config` — modify configuration (auth_admin)
- `org.dasbackup.index` — index snapshots / write DB (auth_admin_keep)
- `org.dasbackup.health` — query SMART data (auth_admin_keep)

**D-Bus methods** (mirror privileged CLI operations):
- `BackupRun(mode: string, sources: string[], targets: string[]) -> job_id: u64`
- `BackupSnapshot(sources: string[]) -> job_id: u64`
- `BackupSend(sources: string[], targets: string[]) -> job_id: u64`
- `BackupBootArchive() -> job_id: u64`
- `IndexWalk(target: string) -> job_id: u64`
- `RestoreFiles(snapshot: string, files: string[], dest: string) -> job_id: u64`
- `RestoreSnapshot(snapshot: string, dest: string) -> job_id: u64`
- `ConfigGet() -> toml: string`
- `ConfigSet(toml: string) -> success: bool`
- `ScheduleGet() -> json: string`
- `ScheduleSet(incremental: string, full: string, delay: u32) -> success: bool`
- `ScheduleEnable(enable: bool) -> success: bool`
- `SubvolAdd(source: string, name: string, manual_only: bool) -> success: bool`
- `SubvolRemove(source: string, name: string) -> success: bool`
- `SubvolSetManual(source: string, name: string, manual: bool) -> success: bool`
- `HealthQuery() -> json: string`
- `JobCancel(job_id: u64) -> success: bool`

**D-Bus signals** (for progress):
- `JobProgress(job_id: u64, stage: string, percent: f64, message: string, throughput_bps: u64, eta_secs: i64)`
- `JobLog(job_id: u64, level: string, message: string)`
- `JobFinished(job_id: u64, success: bool, summary: string)`

### Progress Protocol

The Rust library uses a callback trait for progress reporting:

```rust
pub trait ProgressCallback: Send + Sync {
    fn on_stage(&self, stage: &str, total_steps: u64);
    fn on_progress(&self, current: u64, total: u64, message: &str);
    fn on_throughput(&self, bytes_per_sec: u64);
    fn on_log(&self, level: LogLevel, message: &str);
    fn on_complete(&self, success: bool, summary: &str);
}
```

- **CLI**: Prints to stdout (structured JSON when `--json` flag is set, human-friendly otherwise)
- **D-Bus helper**: Translates callbacks to D-Bus signals
- **FFI layer**: Exposes as C function pointers for GUI to connect to Qt signals
- **GUI**: Connects to signals, renders in progress panel

## 2. Rust Library Refactoring

### Current Structure (lib.rs)
```
pub mod db;
pub mod indexer;
pub mod scanner;
```

### New Structure (lib.rs)
```
pub mod backup;     // NEW: backup orchestration (btrbk wrapper, snapshot, send/receive, boot archive)
pub mod config;     // MOVED from setup/config.rs to lib (shared between CLI and helper)
pub mod db;         // EXISTING: SQLite database operations
pub mod health;     // NEW: SMART monitoring, disk space, target availability
pub mod indexer;    // EXISTING: snapshot discovery and indexing
pub mod progress;   // NEW: progress callback trait and types
pub mod report;     // NEW: email reports, backup history
pub mod restore;    // NEW: file-level and snapshot-level restore
pub mod scanner;    // EXISTING: directory walking
pub mod schedule;   // NEW: systemd timer management, next-run calculation
pub mod subvol;     // NEW: subvolume CRUD, manual-only flagging
```

### Config Data Model Changes

The `Source.subvolumes` field changes from `Vec<String>` to `Vec<SubvolConfig>`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubvolConfig {
    pub name: String,
    #[serde(default)]
    pub manual_only: bool,  // excluded from scheduled backups
}
```

Backward compatibility: The deserializer accepts both formats — bare strings auto-convert to `SubvolConfig { name, manual_only: false }`. A custom `serde` deserializer handles this.

### New DB Tables

```sql
-- Backup run history for statistics dashboard
CREATE TABLE IF NOT EXISTS backup_runs (
    id          INTEGER PRIMARY KEY,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,
    mode        TEXT NOT NULL,  -- 'incremental', 'full', 'manual', 'snapshot-only', 'send-only'
    sources     TEXT,           -- JSON array of source labels
    targets     TEXT,           -- JSON array of target labels
    success     INTEGER NOT NULL DEFAULT 0,
    bytes_sent  INTEGER NOT NULL DEFAULT 0,
    error       TEXT,
    log         TEXT            -- full log output
);

-- Per-target space tracking
CREATE TABLE IF NOT EXISTS target_usage (
    id          INTEGER PRIMARY KEY,
    target_label TEXT NOT NULL,
    recorded_at  INTEGER NOT NULL,
    total_bytes  INTEGER NOT NULL,
    used_bytes   INTEGER NOT NULL,
    free_bytes   INTEGER NOT NULL
);
```

## 3. CLI Expansion

### New Subcommands

All new subcommands support `--json` for machine-readable output.

```
btrdasd backup run [--full|--incremental] [--source LABEL]... [--target LABEL]... [--dry-run] [--json]
    Run a complete backup pipeline. Without flags, runs an incremental backup
    of all sources to all available targets.

    Examples:
      btrdasd backup run                          # incremental, all sources, all targets
      btrdasd backup run --full                    # full backup
      btrdasd backup run --source nvme --target primary-22tb   # selective
      btrdasd backup run --dry-run                 # show what would happen

btrdasd backup snapshot [--source LABEL]... [--json]
    Create btrbk snapshots only (no send/receive).

    Examples:
      btrdasd backup snapshot                      # all sources
      btrdasd backup snapshot --source nvme        # just nvme source

btrdasd backup send [--source LABEL]... [--target LABEL]... [--json]
    Send existing snapshots to targets (no new snapshot creation).

btrdasd backup boot-archive [--json]
    Archive boot subvolumes (@, @home) as read-only snapshots.

btrdasd backup report [--json]
    Generate and optionally email a backup status report.

btrdasd restore file <SNAPSHOT> <PATH>... --dest <DIR> [--json]
    Restore specific files from a snapshot.

    Examples:
      btrdasd restore file "nvme/root.20260221T0304" /etc/fstab --dest /tmp/restore
      btrdasd restore file "nvme/root.20260221T0304" /home/user/.config --dest ~/restore

btrdasd restore snapshot <SNAPSHOT> --dest <DIR> [--json]
    Restore an entire snapshot to a directory.

btrdasd restore browse <SNAPSHOT> [--path <PREFIX>] [--json]
    List files in a snapshot (like 'list' but with better formatting).

btrdasd schedule show [--json]
    Show current backup schedule, next run times, and timer status.

btrdasd schedule set [--incremental <TIME>] [--full <DAYANDTIME>] [--delay <MINUTES>]
    Modify backup schedule.

    TIME format: HH:MM (24-hour)
    DAYANDTIME format: "Day HH:MM" (e.g., "Sun 04:00")

    Examples:
      btrdasd schedule set --incremental 02:30
      btrdasd schedule set --full "Sat 05:00" --delay 15
      btrdasd schedule set --incremental 03:00 --full "Sun 04:00" --delay 30

btrdasd schedule enable
    Enable scheduled backups (start systemd timers).

btrdasd schedule disable
    Disable scheduled backups (stop systemd timers).

btrdasd schedule next [--json]
    Show when the next scheduled backup will run.

btrdasd subvol list [--json]
    List all configured subvolumes, their source, and schedule status.

btrdasd subvol add --source <LABEL> --name <NAME> [--manual-only]
    Add a subvolume to a source's backup list.

    Examples:
      btrdasd subvol add --source nvme --name @var
      btrdasd subvol add --source hdd-projects --name NewProject --manual-only

btrdasd subvol remove --source <LABEL> --name <NAME>
    Remove a subvolume from a source's backup list.

btrdasd subvol set-manual --source <LABEL> --name <NAME>
    Exclude a subvolume from scheduled backups (manual-only).

btrdasd subvol set-auto --source <LABEL> --name <NAME>
    Include a subvolume in scheduled backups.

btrdasd health [--json]
    Show system health: SMART status for DAS drives, target space, backup age,
    growth trends, timer status.

btrdasd config edit <KEY> <VALUE>
    Programmatically set a config value.

    KEY uses dot notation: section.field or section[index].field

    Examples:
      btrdasd config edit schedule.incremental "02:30"
      btrdasd config edit email.enabled true
      btrdasd config edit "target[0].retention.daily" 365
```

### Enhanced --help

Every subcommand gets `long_about` with:
- Full description of what the command does
- Argument format specifications
- 2-3 usage examples
- Cross-references to related commands

### --json Flag

Global flag available on all read commands. Output format:

```json
{
  "version": "0.6.0",
  "command": "info",
  "data": { ... },
  "timestamp": "2026-02-24T09:30:00Z"
}
```

For long-running operations, progress events are newline-delimited JSON:

```json
{"type":"progress","stage":"Sending @home","percent":45.2,"throughput_bps":12884901888,"eta_secs":180}
{"type":"log","level":"info","message":"Snapshot nvme/root.20260224T0300 sent to primary-22tb"}
{"type":"complete","success":true,"summary":"2 snapshots sent, 24.5 GB transferred"}
```

## 4. GUI Expansion

### Main Window Redesign

```
┌─ ButteredDASD ──────────────────────────────────────────────────────────┐
│ [File] [Backup] [Tools] [Settings] [Help]                              │
├─────────────────────────────────────────────────────────────────────────┤
│ ┌───────────┐ ┌──────────────────────────────────────────────────────┐  │
│ │ NAVIGATION│ │ CENTRAL AREA (context-dependent)                    │  │
│ │           │ │                                                      │  │
│ │ ▶ Browse  │ │  ┌─────────────────────────────────────────────┐    │  │
│ │   Search  │ │  │ Breadcrumb: /mnt/backup-22tb/nvme/root...  │    │  │
│ │           │ │  ├─────────────────────────────────────────────┤    │  │
│ │ ▶ Backup  │ │  │ Name      Size    Modified    Type         │    │  │
│ │   Run Now │ │  │ ─────     ─────   ────────    ────         │    │  │
│ │   History │ │  │ etc/      -       2026-02-24  Directory    │    │  │
│ │           │ │  │ home/     -       2026-02-24  Directory    │    │  │
│ │ ▶ Config  │ │  │ fstab     1.2K    2026-02-20  File         │    │  │
│ │   Sources │ │  │ passwd    2.4K    2026-02-18  File         │    │  │
│ │   Targets │ │  │                                             │    │  │
│ │   Schedule│ │  └─────────────────────────────────────────────┘    │  │
│ │   ESP     │ │                                                      │  │
│ │   Email   │ │  ┌─────────────────────────────────────────────┐    │  │
│ │           │ │  │ SEARCH RESULTS (visible when searching)     │    │  │
│ │ ▶ Health  │ │  └─────────────────────────────────────────────┘    │  │
│ │   Drives  │ │                                                      │  │
│ │   Growth  │ └──────────────────────────────────────────────────────┘  │
│ │   Status  │                                                           │
│ └───────────┘                                                           │
├─────────────────────────────────────────────────────────────────────────┤
│ ▼ Progress Panel (collapsible dock)                                     │
│ ┌───────────────────────────────────────────────────────────────────┐   │
│ │ [Sending @home → primary-22tb]  ████████████░░░░  72%  14 GB/s   │   │
│ │ ┌ Raw Log ──────────────────────────────────────────────────────┐ │   │
│ │ │ [09:31:02] btrbk send @home.20260224T0300 -> /mnt/backup... │ │   │
│ │ │ [09:31:03] 8,234 files, 24.5 GB ...                         │ │   │
│ │ └──────────────────────────────────────────────────────────────┘ │   │
│ └───────────────────────────────────────────────────────────────────┘   │
├─────────────────────────────────────────────────────────────────────────┤
│ ⓘ Next backup: Sun 04:00 (in 18h) │ 3 targets online │ DB: 2.1 GB    │
└─────────────────────────────────────────────────────────────────────────┘
```

### Navigation Sidebar

Left sidebar with collapsible sections. Each section expands to sub-items. Selecting a sub-item changes the central area.

- **Browse** — Snapshot timeline (existing) + file browser (new)
  - Snapshots — existing timeline view
  - Search — FTS5 search (existing)
- **Backup** — Backup operations
  - Run Now — manual backup with operation picker
  - History — past backup runs (table from backup_runs DB table)
- **Config** — Configuration editor
  - Sources — source volumes/subvolumes (add/remove/manual-only toggle)
  - Targets — backup targets (add/remove, retention per target)
  - Schedule — time/day picker, enable/disable, next-run display
  - ESP — partition picker, mirror toggle, hook config
  - Email — SMTP settings, test email button
  - General — DB path, log file, install prefix, DAS settings
- **Health** — Monitoring dashboard
  - Drives — SMART status for each DAS drive
  - Growth — chart of disk usage over time (from growth.log + target_usage table)
  - Status — btrbk status, timer status, target availability

### File Browser Widget (Dolphin-style)

**Based on**: KDirModel + KDirLister pattern (snapshots are just directories on disk, so KIO can browse them directly when mounted)

**Components:**
- **Breadcrumb bar** (KUrlNavigator) — click-to-navigate path segments
- **View modes** — icon view, detail view, tree view (toggle buttons in toolbar)
- **Detail view columns**: Name, Size, Modified, Type, Permissions
- **Sorting** — click column headers to sort
- **Multi-select** — Ctrl+click, Shift+click, rubber-band selection
- **Context menu**: Restore to..., Restore to original location, Compare with current version, Copy path, Properties
- **Preview panel** (optional, toggleable) — file preview for text/images on the right side
- **Filter bar** — inline text filter that narrows the current directory listing
- **Navigation**: Back/Forward buttons, Up button, Home (snapshot root)

**For snapshots not directly mountable** (already sent to target), the file browser falls back to the DB-backed model (existing FileModel) which shows files from the indexed data.

### Backup Operations Panel ("Run Now")

When user selects "Run Now" from navigation:

```
┌─ Run Backup ────────────────────────────────────────────────┐
│                                                              │
│  Mode: ○ Incremental (default)  ○ Full                      │
│                                                              │
│  Operations:                                                 │
│    ☑ Create snapshots                                        │
│    ☑ Send to targets                                         │
│    ☑ Boot archive                                            │
│    ☑ Index new snapshots                                     │
│    ☑ Email report                                            │
│                                                              │
│  Sources:                        Targets:                    │
│    ☑ nvme (@, @home, @root, @log)  ☑ primary-22tb (Bay 2)  │
│    ☑ ssd (@opt, @srv)              ☑ system-2tb (Bay 6)    │
│    ☑ hdd-projects (ClaudeCode...)   ☑ system-mirror (Bay 1) │
│    ☑ hdd-audiobooks (Audiobooks)                             │
│    ☑ das-storage (@data)                                     │
│                                                              │
│  [Dry Run]  [Run Backup]                                     │
└──────────────────────────────────────────────────────────────┘
```

Checkboxes allow cherry-picking individual operations, sources, and targets. Manual-only subvolumes appear grayed with a note; checking them overrides for this run.

### Config Editor (Tabbed Dialog)

Replaces the current minimal SettingsDialog. Uses KPageDialog with icon list on the left.

**Tabs:**
1. **Sources** — Table of sources. Each row: label, volume, device, subvolume list. Add/Remove buttons. Clicking a source opens sub-editor for its subvolumes (table with name + manual_only checkbox + remove button + add button).
2. **Targets** — Table of targets. Each row: label, serial, mount, role (dropdown), display name. Inline retention editor (daily/weekly/monthly/yearly spinboxes).
3. **Schedule** — Time picker for incremental (QTimeEdit), day+time picker for full (QComboBox + QTimeEdit), delay spinbox. Enable/Disable toggle. Shows "Next run: ..." computed from systemd timer.
4. **ESP** — Checkbox for enable. Partition list (add/remove). Mirror checkbox. Hook type dropdown.
5. **Email** — Enable checkbox. SMTP host/port/from/to fields. Auth dropdown. "Send Test Email" button.
6. **Boot** — Enable checkbox. Subvolume list for boot archival. Retention days spinbox.
7. **DAS** — Model pattern, IO scheduler, mount options fields.
8. **General** — DB path, log file, growth log, btrbk conf path, install prefix. All with file browse buttons.

**Save behavior**: Writes config via D-Bus helper (privileged). Validates before saving. Shows diff of changes before confirming.

### First-Run Wizard

KAssistantDialog (KDE wizard framework) with pages matching the CLI wizard steps:

1. Welcome + dependency check
2. Backup sources (subvolume picker)
3. Backup targets (DAS drive picker)
4. ESP configuration
5. Retention policy
6. Schedule
7. Email notifications
8. Install location
9. GUI preferences
10. Review + confirm

Launches automatically on first run (no config.toml found) or via menu: Settings > Setup Wizard.

### Progress Panel

QDockWidget at the bottom of the main window. Collapsible (starts collapsed when no operation running).

**When an operation is running:**
- **Header**: Operation name + stage + overall progress bar + percent + throughput + ETA
- **Sub-progress**: Per-source or per-snapshot progress when applicable
- **Raw log**: Collapsible QPlainTextEdit with auto-scroll, monospace font. Shows timestamped log lines from the D-Bus JobLog signal.
- **Cancel button**: Sends JobCancel via D-Bus

**When no operation:**
- Shows last operation summary (time, result, duration)
- Or "No recent operations"

### Health Dashboard

Three sub-views:

**Drives:**
```
┌─ Drive Health ──────────────────────────────────────────────┐
│ Drive               Status    Temp    Hours    Errors       │
│ ────────            ──────    ────    ─────    ──────       │
│ Bay 1 (ZK208Q77)   ● OK      34°C    12,450   0           │
│ Bay 2 (ZXA0LMAE)   ● OK      36°C    8,200    0           │
│ Bay 6 (ZFL41DNY)   ● OK      33°C    15,100   0           │
│ NVMe (root)        ● OK      42°C    5,600    0           │
└──────────────────────────────────────────────────────────────┘
```

**Growth:**
- QChart (Qt Charts) line graph showing disk usage over time per target
- Data from growth.log + target_usage table
- Time range selector (1 week, 1 month, 3 months, 1 year, all)

**Status:**
- btrbk status (last run, next run)
- Timer status (enabled/disabled, next trigger)
- Target availability (mounted/unmounted, space remaining)
- Last backup age per source (warning if stale)
- Index freshness (last walk time)

### Desktop Integration

- **KNotification**: Desktop notifications on backup complete/fail, SMART warnings, target space low
- **System tray** (optional, KStatusNotifierItem): Background indicator during backup, quick access to trigger backup or view status
- **KHelpMenu**: Standard KDE help menu with handbook, about dialog
- **KAboutData**: Proper application metadata

## 5. Documentation

### Man Page (btrdasd.1)

Full Unix man page covering all subcommands. Installed to `/usr/share/man/man1/btrdasd.1`.

Sections:
- NAME
- SYNOPSIS
- DESCRIPTION
- COMMANDS (each subcommand with full options, arguments, examples)
- CONFIGURATION (config.toml format, all sections documented)
- FILES (/etc/das-backup/config.toml, /var/lib/das-backup/backup-index.db, etc.)
- ENVIRONMENT (BTRDASD_DB, BTRDASD_CONFIG override variables)
- EXIT STATUS
- EXAMPLES (common workflows: first setup, manual backup, restore file, check health)
- SEE ALSO (btrbk, btrfs, smartctl)

Source format: `roff` (direct troff macros) for maximum compatibility.

### Info Page

GNU Texinfo document with tutorial-style organization:
- Getting Started
- Configuration Guide
- Backup Operations
- Restore Operations
- Scheduling
- Monitoring and Health
- Troubleshooting
- Reference (all commands)

### Rich --help

Every subcommand uses clap's `long_about`, `long_help`, and `after_help` to include:
- Detailed description
- Format specifications for arguments
- 2-3 examples
- Cross-references

### HTML Documentation

Generated from the man page via `groff -mandoc -Thtml` or maintained separately as a lightweight HTML guide. Installed to `/usr/share/doc/das-backup/`.

## 6. Subvolume Management

### Data Model

```rust
// In config.rs — replaces Vec<String>
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SubvolEntry {
    Simple(String),                    // backward-compat: bare string
    Full(SubvolConfig),                // new: struct with manual_only
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubvolConfig {
    pub name: String,
    #[serde(default)]
    pub manual_only: bool,
}
```

### Config.toml Format

```toml
[[source]]
label = "nvme"
volume = "/.btrfs-nvme"
device = "/dev/nvme1n1p2"
snapshot_dir = ".btrbk-snapshots"
target_subdirs = ["nvme"]

# New format: array of tables for subvolumes
[[source.subvolumes]]
name = "@"

[[source.subvolumes]]
name = "@home"

[[source.subvolumes]]
name = "@root"
manual_only = true    # excluded from scheduled backups

[[source.subvolumes]]
name = "@log"
```

Backward compatibility: Simple string arrays still parse (via `#[serde(untagged)]`).

### CLI Operations

```bash
# List subvolumes with their status
$ btrdasd subvol list
SOURCE          SUBVOLUME    SCHEDULE
nvme            @            auto
nvme            @home        auto
nvme            @root        manual-only
nvme            @log         auto
ssd             @opt         auto
ssd             @srv         auto
hdd-projects    ClaudeCode.. auto

# Add a new subvolume
$ sudo btrdasd subvol add --source nvme --name @var
Added @var to source 'nvme' (scheduled: auto)

# Flag as manual-only
$ sudo btrdasd subvol set-manual --source nvme --name @root
Set @root to manual-only (excluded from scheduled backups)

# Remove
$ sudo btrdasd subvol remove --source nvme --name @var
Removed @var from source 'nvme'
```

### GUI

In the Sources tab of the Config dialog, each source has an expandable subvolume table:

| Subvolume | Scheduled | Actions |
|-----------|-----------|---------|
| @ | ☑ Auto | [Remove] |
| @home | ☑ Auto | [Remove] |
| @root | ☐ Manual | [Remove] |
| [+ Add Subvolume] | | |

Checkbox toggles between auto (included in schedule) and manual-only. Manual-only subvolumes appear in the "Run Now" panel but are unchecked by default.

## 7. Additional UX Enhancements

### GUI Polish
- **Dark/light theme**: Follows KDE system theme automatically (Qt6 handles this)
- **Keyboard shortcuts**: Ctrl+B (run backup), Ctrl+R (restore), Ctrl+F (search), Ctrl+, (settings), F5 (refresh)
- **Drag and drop**: Drag files from snapshot browser to Dolphin/desktop for restore
- **Undo for config changes**: Before saving, show a diff and allow cancel
- **Backup dry-run**: Visible in GUI, shows what would happen without executing
- **Toast notifications**: Inline status messages (KMessageWidget) for quick feedback
- **Responsive layout**: Splitter-based layout works at various window sizes
- **Recent operations**: Dashboard widget showing last 5 operations with status/duration
- **Column auto-resize**: File browser columns auto-fit content width

### CLI Polish
- **Color output**: Colored status indicators (green=ok, yellow=warning, red=error) via `console` crate (already a dependency)
- **Table formatting**: Aligned columns for list/status output via `comfy-table` or similar
- **Confirmation prompts**: Destructive operations (remove subvol, uninstall) require confirmation; `--yes` flag to bypass
- **Quiet mode**: `--quiet` flag suppresses non-error output
- **Verbose mode**: `-v`/`-vv` for increasing verbosity
- **Pager**: Long output (health report, file listings) piped through `$PAGER` when stdout is a terminal
- **Shell completions**: Generated by clap for bash, zsh, fish. Installed to appropriate completion dirs

### Error Messages
- Human-readable errors with context (not just "permission denied" but "Cannot write to database at /var/lib/das-backup/backup-index.db: permission denied. Run with sudo or add your user to the 'backup' group.")
- Suggestions for common fixes
- Error codes for scripting (`--json` mode includes error codes)

## 8. Build System Changes

### CMakeLists.txt Updates

- New ExternalProject target for `btrdasd-helper` (Rust, separate binary)
- New ExternalProject target for `libbuttered_dasd_ffi` (Rust cdylib)
- GUI links against `libbuttered_dasd_ffi.so` in addition to Qt6/KF6
- New install targets: man page, info page, HTML docs, polkit policy, D-Bus service file, shell completions
- New option: `BUILD_HELPER` (default ON)
- New option: `BUILD_DOCS` (default ON)

### New Install Artifacts

```
/usr/bin/btrdasd                          # CLI (existing)
/usr/bin/btrdasd-gui                      # GUI (existing)
/usr/libexec/btrdasd-helper               # D-Bus helper (new)
/usr/lib/libbuttered_dasd_ffi.so          # FFI library (new)
/usr/share/dbus-1/system-services/org.dasbackup.Helper1.service  # D-Bus activation (new)
/usr/share/dbus-1/system.d/org.dasbackup.Helper1.conf            # D-Bus policy (new)
/usr/share/polkit-1/actions/org.dasbackup.policy                 # Polkit rules (new)
/usr/share/man/man1/btrdasd.1             # Man page (new)
/usr/share/info/btrdasd.info              # Info page (new)
/usr/share/doc/das-backup/index.html      # HTML docs (new)
/usr/share/bash-completion/completions/btrdasd   # Shell completions (new)
/usr/share/zsh/site-functions/_btrdasd           # Zsh completions (new)
/usr/share/fish/vendor_completions.d/btrdasd.fish # Fish completions (new)
```

## 9. Version and Migration

- **Version bump**: 0.5.0 → 0.6.0
- **Config migration**: `btrdasd setup --upgrade` detects old-format subvolume arrays and converts to new SubvolConfig format automatically
- **DB migration**: New tables (backup_runs, target_usage) created on first access via `CREATE TABLE IF NOT EXISTS`
- **No breaking CLI changes**: All existing commands (`walk`, `search`, `list`, `info`, `setup`, `config`) retain their current behavior
