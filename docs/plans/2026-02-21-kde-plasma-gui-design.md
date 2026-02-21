# ButteredDASD KDE Plasma GUI — Design Document

**Date**: 2026-02-21
**Status**: Approved
**Component**: `gui/` — C++20 / Qt6 / KDE Frameworks 6

## Goal

Build a native KDE Plasma application for searching, browsing, and restoring files from BTRFS backup snapshots indexed by the ButteredDASD content indexer (`btrdasd`).

## Architecture

Hybrid approach: KF6 widgets for the main application shell (KXmlGuiWindow, KIO, KConfigDialog) with a custom-painted `QWidget` for the snapshot timeline visualization. The GUI reads the existing SQLite FTS5 database (`backup-index.db`) in read-only mode via `QSqlDatabase`. It can trigger indexing by spawning `btrdasd walk` as a subprocess and auto-detects new snapshots via `QFileSystemWatcher`.

## Tech Stack

- **C++20**: std::format, concepts, ranges, designated initializers
- **Qt6 6.10.2**: Widgets, Sql, Core
- **KDE Frameworks 6.23.0**: KXmlGui, KI18n, KIO, KCoreAddons, KConfigWidgets, KIconThemes, KCrash
- **ECM 6.23.0**: Extra CMake Modules for KDE integration
- **CMake 4.2.3**: Build system
- **SQLite 3.51.2**: Via Qt6::Sql QSQLITE driver (FTS5, WAL mode)

## Application Identity

- **Name**: ButteredDASD
- **Binary**: `btrdasd-gui`
- **Desktop entry**: Shows as "ButteredDASD" in the application menu
- **Icon**: Custom (TBD — use a generic backup icon initially)

---

## Layout

```
┌─────────────────────────────────────────────────────────────┐
│ ButteredDASD                                    [_][□][×]   │
├──────────────────────────┬──────────────────────────────────┤
│ Toolbar: [Search...    ] │ [Re-index] [Restore] [Stats]    │
├──────────────────────────┴──────────────────────────────────┤
│                                                             │
│  ┌─ Snapshot Timeline ──┐  ┌─ File List ──────────────────┐│
│  │                      │  │                              ││
│  │  ▼ 2026-02-21       │  │  Name    Size   Modified     ││
│  │    nvme/root         │  │  ────    ────   ────────     ││
│  │    nvme/home         │  │  .zshrc  1.2K   Feb 21 03:04││
│  │    sata/data         │  │  .bashrc 512B   Feb 20 03:04││
│  │                      │  │  ...                         ││
│  │  ▼ 2026-02-20       │  │                              ││
│  │    nvme/root         │  │                              ││
│  │    nvme/home         │  │                              ││
│  │                      │  │                              ││
│  └──────────────────────┘  └──────────────────────────────┘│
│                                                             │
├─────────────────────────────────────────────────────────────┤
│  Search Results / Details Panel (toggleable)                │
│  ┌──────────────────────────────────────────────────────────┤
│  │ report.pdf  │ 15KB │ nvme/root │ Feb 20 → Feb 25      ││
│  │ report.pdf  │ 18KB │ nvme/root │ Feb 26 → Feb 28      ││
│  │ Path: /mnt/backup-hdd/nvme/root.20260225T0304/docs/... ││
│  └──────────────────────────────────────────────────────────┤
├─────────────────────────────────────────────────────────────┤
│ Status: 42 snapshots │ 158,234 files │ DB: 12.3 MB         │
└─────────────────────────────────────────────────────────────┘
```

**Layout structure**: `QSplitter` (horizontal) with:
- **Left**: Custom `SnapshotTimeline` widget
- **Right**: `QSplitter` (vertical) with file list `QTableView` on top, details/search results panel below

**Bottom**: `QStatusBar` with database stats (snapshot count, file count, DB size).

---

## Custom Snapshot Timeline Widget

The `SnapshotTimeline` is a custom `QWidget` with its own `paintEvent`. It is the signature visual element.

### Visual Design

- Vertical timeline line (2px, KDE accent color) running down the left edge
- Date headers as rounded pills (background: theme surface color, text: bold)
- Snapshot nodes: small circle on the timeline, label showing `source/name`
- Selected node: filled circle + highlight background
- Hover: tooltip with full path and indexed-at timestamp
- Smooth scroll via `QScrollArea`

```
   ●━━ Feb 21, 2026 ━━━━━━━━━━━━━━
   │
   ├─● nvme/root.20260221T0304
   ├─● nvme/home.20260221T0304
   └─● sata/data.20260221T0304

   ●━━ Feb 20, 2026 ━━━━━━━━━━━━━━
   │
   ├─● nvme/root.20260220T0304
   └─● nvme/home.20260220T0304
```

### Interaction

- Click snapshot node → populates file list (right panel)
- Click date header → selects/expands all snapshots for that date
- Right-click → context menu: "List files", "Re-index this snapshot"

### Data Source

`SnapshotModel : QAbstractItemModel` queries `SELECT * FROM snapshots ORDER BY ts DESC` and groups by date in C++.

---

## Search

- Search bar in the toolbar (KDE-style, like Dolphin's filter bar)
- 300ms debounce before firing FTS5 query (`files_fts MATCH ?`)
- Results appear in the bottom panel `QTableView`
- Columns: Path, Size, Modified, First Snapshot, Last Snapshot
- Double-click result → selects that snapshot in the timeline, shows file in file list
- Supports FTS5 syntax: prefix (`report*`), exact phrase (`"exact match"`), column filter (`name:report`)

---

## File List

`QTableView` backed by `FileModel : QAbstractTableModel`:
- Columns: Name, Path, Size, Modified, Type (icon), Span (e.g., "Feb 20 → Feb 25")
- Sortable via `QSortFilterProxyModel`
- Right-click context menu: "Copy path", "Restore to...", "Show in file manager"

---

## Restore Workflow

1. User selects file(s) in file list or search results
2. Clicks "Restore to..." (toolbar button or context menu)
3. File dialog for destination selection
4. `KIO::copy()` from snapshot path to destination
5. Progress via `KIO::JobTracker` (standard KDE progress dialog)
6. If snapshot not mounted → error dialog with mount path info

Path display: detail panel shows full snapshot path, "Copy path" button copies to `QClipboard`.

---

## Indexing & Auto-Watch

### Re-index Button

- Spawns `btrdasd walk <target>` via `QProcess`
- Target path from settings (default: derived from database snapshot paths, or `/mnt/backup-hdd`)
- Progress dialog shows btrdasd stdout in real-time
- On completion, all models reload from database

### Auto-Watch (QFileSystemWatcher)

- Watches backup target directory for new subdirectories (new snapshots)
- 30-second delay after detection (waits for btrbk to finish writing)
- Triggers `btrdasd walk` automatically
- Configurable: enabled/disabled, watch path
- System tray notification when new snapshots are auto-indexed

---

## Settings (KConfigDialog)

Stored via `KSharedConfig`:
- Database path (default: `/var/lib/das-backup/backup-index.db`)
- Backup target path(s) for re-indexing
- Auto-watch: enabled/disabled + watch path
- Default restore destination

---

## Database Connection

- `QSqlDatabase::addDatabase("QSQLITE")` with WAL mode
- Read-only for all GUI queries
- Path configurable via `--db` CLI flag
- Connection persists for application lifetime
- `PRAGMA optimize` on close

---

## Component Architecture

| File | Class | Purpose |
|------|-------|---------|
| `main.cpp` | — | KAboutData, KLocalizedString, app setup |
| `mainwindow.h/cpp` | `MainWindow : KXmlGuiWindow` | Shell, toolbar, splitters, status bar |
| `snapshotmodel.h/cpp` | `SnapshotModel : QAbstractItemModel` | Timeline data model (snapshots table) |
| `snapshottimeline.h/cpp` | `SnapshotTimeline : QWidget` | Custom-painted timeline widget |
| `filemodel.h/cpp` | `FileModel : QAbstractTableModel` | File list model (files + spans) |
| `searchmodel.h/cpp` | `SearchModel : QAbstractTableModel` | FTS5 search results model |
| `database.h/cpp` | `Database` | SQLite connection wrapper |
| `indexrunner.h/cpp` | `IndexRunner : QObject` | QProcess wrapper for btrdasd |
| `snapshotwatcher.h/cpp` | `SnapshotWatcher : QObject` | QFileSystemWatcher + auto-index |
| `settingsdialog.h/cpp` | `SettingsDialog : KConfigDialog` | Settings UI |
| `restoreaction.h/cpp` | `RestoreAction : QObject` | KIO::copy job wrapper |

All source files in `gui/src/`. Build via `gui/CMakeLists.txt` using ECM.

---

## Testing Strategy

- **Unit tests** (`QTest`): SnapshotModel, FileModel, SearchModel — using in-memory SQLite
- **Integration test**: IndexRunner spawning btrdasd with a temp directory
- **Manual testing**: SnapshotTimeline widget painting, full UX flow
- No GUI integration tests initially (complex, diminishing returns)

---

## Error Handling

| Scenario | Response |
|----------|----------|
| Database open failure | `KMessageBox::error` with path info |
| btrdasd not found | Error dialog with install instructions |
| Restore failure | KIO error dialog (standard KDE behavior) |
| Snapshot not mounted | Informational dialog with mount command |
| FTS5 query syntax error | Show "Invalid search" in status bar, clear results |

---

## Dependencies

### Build Dependencies
- CMake 4.2.3+, ECM 6.23.0+
- Qt6: Core, Widgets, Sql, Test
- KF6: XmlGui, I18n, KIO, CoreAddons, ConfigWidgets, IconThemes, Crash

### Runtime Dependencies
- `btrdasd` binary (for re-indexing)
- SQLite database at configured path
- BTRFS snapshots mounted (for restore operations)
