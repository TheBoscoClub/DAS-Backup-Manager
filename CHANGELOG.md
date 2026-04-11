# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

### Changed

### Fixed

## [0.7.12.1] - 2026-04-11

### Fixed
- **Debian `.deb` artifacts shipped with stale `0.7.10` version on the v0.7.12 release** — `packaging/debian/changelog` had not been bumped since 0.7.10, so `dpkg-buildpackage` stamped the artifacts with that version even though `CMakeLists.txt`, `Cargo.toml`, and the release tag all said 0.7.12. The `upload-release` step attached the mis-versioned debs to the v0.7.12 GitHub Release. Fix: inject a fresh top entry in `debian/changelog` at build time from `$VERSION_TAG` inside the debian job, so the artifacts always match the tag regardless of the committed changelog state

## [0.7.12] - 2026-04-11

### Changed
- **`release-packages.yml` workflow — all 6 packaging formats repaired end-to-end** — the workflow had been broken across 5 consecutive tagged releases, with every format failing for independent reasons (stale `kf6-` dependency prefixes in Arch, missing `systemd-rpm-macros` in Fedora, outdated rustc in the Debian and AppImage containers, a `FIXME` sha256 placeholder in the Flatpak manifest, and an unresolvable KF6 toolchain on the Snap `core24` base). The workflow now builds Arch, Debian, Fedora, AppImage, and Flatpak green from a single `workflow_dispatch` test run. See `### Removed` below for the Snap format disposition
- **`release-packages.yml` now triggers on both `push: tags: ['v*']` and `workflow_dispatch`** — manual dispatch accepts a `version` input so the workflow can be iterated without cutting a real tag. `upload-release` is gated on `github.event_name == 'push' && startsWith(github.ref, 'refs/tags/')` so dispatch runs skip the GitHub Release upload step
- **Debian and AppImage containers now install rustup inline** — Debian trixie ships `rustc 1.85.0`, but `zbus 5.14` and `zvariant 5.10` require `1.87` and the `buttered-dasd` lib crate uses `let`-chain expressions that require `1.88`. Both jobs now install the stable toolchain via `sh.rustup.rs` and prepend `~/.cargo/bin` to `$GITHUB_PATH` so the newer compiler shadows the apt-packaged one during the actual build
- **Arch and Fedora jobs sanitize hyphens in version strings** — `pkgver` and RPM `Version:` both forbid `-`, so test/RC versions like `0.7.12-test` are rewritten to `0.7.12_test` before being sed'd into `PKGBUILD`/`.spec`
- **Flatpak container now runs with `--privileged`** — `bwrap` inside the `bilelmoussaoui/flatpak-github-actions:kde-6.7` container cannot create user namespaces without elevated privileges on the GitHub runner, which blocked every previous Flatpak build at `module qtcharts: Child process exited with code 1`
- **Flatpak manifest source type changed from `archive` to `dir`** — the old `archive` source pinned a release tarball with a `FIXME` sha256 placeholder, which meant every release manifest had to be hand-edited before it could build. The manifest now uses the local checkout (`type: dir, path: ../..`) so it builds from the tagged tree without round-tripping through GitHub's archive endpoint
- **AppImage build sets `APPIMAGE_EXTRACT_AND_RUN=1`** — FUSE is unavailable inside the Debian trixie Docker container, so `linuxdeploy` and `appimagetool` now extract themselves instead of mounting via FUSE
- **Fedora spec file gained `BuildRequires: systemd-rpm-macros`** — required so `%{_unitdir}` macro expands when the `%files` section is evaluated. The Fedora job's `dnf install` list now also pulls in the package
- **Debian `debian/rules` simplified** — removed the `. $(HOME)/.cargo/env 2>/dev/null;` prelude from `override_dh_auto_configure` and `override_dh_auto_build`. Under `debhelper`'s `set -e` shell, sourcing a non-existent file aborted the build. The rustup PATH is now threaded through `$GITHUB_PATH` at the job level instead
- **Arch `PKGBUILD` KF6 dependency names updated** — Arch renamed `kf6-kcoreaddons` etc. to the unprefixed `kcoreaddons` family some time ago; `makepkg -s` was failing to resolve the old names

### Removed
- **Snap packaging temporarily disabled in `release-packages.yml`** — `snapcraft` has no snap `base` that provides a working KF6 toolchain. `core24` (Ubuntu 24.04 noble) does not package KF6, and there is no newer stable base available. The job is commented out with a note to re-enable once `core26` (Ubuntu 26.04 LTS) ships as a snap base. The `packaging/snap/snapcraft.yaml` manifest is retained unchanged so it is ready to re-enable
- **`render_esp_hook()` template generator** — root cause of the 2026-03-05 DAS ESP wipe incident. The function in `indexer/src/setup/templates.rs` generated a pacman hook at `/etc/pacman.d/hooks/das-esp-sync.hook` that called `/usr/lib/das-backup/esp-sync.sh` (a script no longer present in the repo). The script discovered ESP partitions by label and mirrored the host ESP onto all of them — including `BACKUP-ESP` on the DAS 2TB emergency recovery drives, destroying their independent OS boot configurations. The function kept regenerating the orphan hook on every `btrdasd setup --upgrade` run, leaving a latent vector for the disaster to recur
- **`EspHooks` struct and `HookType` enum** from `indexer/src/config.rs` — no remaining consumers after hook generator removal. Any `[esp.hooks]` block in an older `config.toml` is silently ignored via serde's default handling
- **ESP hook auto-detect and prompt** from `indexer/src/setup/wizard.rs` — the interactive wizard no longer offers to install any package-manager ESP hook
- **`/etc/pacman.d/hooks/das-esp-sync.hook`** deleted from the live system
- **`[esp.hooks]` block** removed from `/etc/das-backup/config.toml`

### Fixed
- **2026-03-05 DAS ESP wipe incident — permanent fix** — DAS-Backup-Manager no longer contains any code path that can generate an ESP sync hook, pacman or otherwise. The independent NVMe-pair mirror sync (`/usr/local/bin/esp-sync.sh` via `esp-mirror.hook`) is a completely separate mechanism outside this project and continues to function normally. See `.claude/rules/das-esp-safety.md` for the full postmortem and the hard rule

## [0.7.11] - 2026-03-16

### Fixed
- **GUI: `&` accelerator markers corrupting source/target names sent to backend** — KDE's KAcceleratorManager auto-inserts `&` into checkbox text; `QCheckBox::text()` returned these markers (e.g., `"&primary-22tb"`) causing btrbk target mismatch warnings. Now stores original labels via `QWidget::setProperty()` and reads those back instead
- **GUI: Backup Operations panel content invisible when progress dock expanded** — Mode, Operations, Sources, and Targets group boxes collapsed to title-only height because the progress dock consumed all vertical space. Wrapped panel content in `QScrollArea` so checkboxes remain accessible regardless of dock size
- **GUI: Status bar stuck on "Loading..." indefinitely** — If any of the 3 async D-Bus status queries (IndexStats, ScheduleGet, HealthQuery) hung, the pending counter never reached zero. Added 10-second timeout fallback that assembles status bar with whatever data has arrived

## [0.7.10] - 2026-03-16

### Added
- **`btrdasd backup record-run` subcommand** — Allows external scripts (backup-run.sh) to record backup runs in the database for GUI history display

### Fixed
- **Backup history not recording from CLI or shell script** — Only GUI-initiated backups (via D-Bus helper) were recorded in `backup_runs` table; CLI `btrdasd backup run` and nightly `backup-run.sh` now both record runs
- **Error messages with commas corrupted in record-run** — Changed from comma-delimited `--errors` to newline-separated string, matching database storage format
- **JSON key naming inconsistency in `backup report --json`** — Changed `snapshots_created`/`snapshots_sent` to `snaps_created`/`snaps_sent` to match D-Bus, FFI, and GUI field naming
- **Failed backups from shell script never recorded** — If `run_btrbk` or any post-backup operation failed, `set -e` + ERR trap would abort before reaching `record_backup_run_in_db`; now the cleanup trap records the failure, with double-recording guard

## [0.7.9] - 2026-03-07

### Removed
- **Dockerfile and Docker references** — Docker containerization is incompatible with DAS backup operations (requires direct BTRFS subvolume access, physical drive access, btrfs send/receive); removed Dockerfile, .dockerignore, and all Docker documentation

## [0.7.8] - 2026-03-07

### Fixed
- **Progress panel dock resize** — Replaced internal `QSplitter` (which could only redistribute within the dock's fixed height) with native QMainWindow dock resizing; central widget minimum height set to 50px so the progress panel dock can expand to consume nearly the full window height by dragging the top edge

## [0.7.7] - 2026-03-07

### Changed
- **Progress panel log view resizable** — Central widget minimum height reduced to 50px so the bottom dock (progress panel) can expand to consume nearly the full window height via the native QMainWindow dock separator; drag the top edge of the panel to resize
- **Smart auto-scroll in log view** — New log entries only auto-scroll to bottom when the user is already at the bottom; scrolling up to inspect earlier entries no longer snaps back on each new line

### Fixed
- **Log disappears after backup completes** — The progress panel no longer auto-hides 5 seconds after job completion; the log stays visible (and auto-expands) so users can review the full output for errors and inconsistencies; the panel can be closed manually via the dock's X button
- **Email failure marks entire backup as "Failed"** — Email report errors were pushed to the backup errors vec, causing successful backups (snapshots created, data sent, boot archived) to show as "Failed" in history; email failure is now a non-fatal warning matching the existing pattern for indexing failures
- **s-nail v14.9+ deprecated variable warnings** — Switched from obsoleted `smtp`/`smtp-auth-user`/`smtp-auth-password`/`ssl-verify` variables to v15-compat mode with `mta=` URL (embedded credentials), `smtp-auth=login`, and `tls-verify` (renamed from `ssl-verify`)

## [0.7.6] - 2026-03-05

### Added
- **Email backup reports** — `report.rs` rewritten with `send_email_report()` via s-nail/mailx and comprehensive `format_report()` matching the original shell script format (header, backup operations, throughput, disk capacity, SMART status, latest snapshots, errors, footer)
- **Journald logging in D-Bus helper** — `btrdasd-helper` now logs all messages to stderr (journald) via `eprintln!` for post-mortem debugging

### Fixed
- **"Email reports not yet integrated — skipping"** — Email sending was stubbed out when orchestration moved from shell to Rust; now fully wired into the backup pipeline using Protonmail Bridge SMTP credentials from `/etc/das-backup-email.conf`
- **btrbk.conf canonical path** — Default config path changed from `/etc/das-backup/btrbk.conf` to `/etc/btrbk/btrbk.conf` (the canonical location btrbk expects); setup template generation updated to match
- **Dry-run backups polluting history** — Dry runs were recording zero-work entries in the `backup_runs` table; now guarded with `if !options.dry_run` in the D-Bus helper
- **Packaging version sync** — All packaging formats (Arch PKGBUILD, Debian control, Fedora spec, Snap) synced to correct version with optional dependencies for s-nail and rsync
- **Script BTRDASD_BIN defaults** — `das-partition-drives.sh` and `boot-archive-cleanup.sh` defaulted to `/usr/local/bin/btrdasd` instead of `/usr/bin/btrdasd` (the cmake install location); fixed both scripts
- **Snap missing runtime dependencies** — Added KF6 runtime libraries (`libkf6*`) and `util-linux` to Snap `stage-packages` for GUI and `lsblk` support
- **Docker missing btrbk and util-linux** — Dockerfile runtime stage lacked `btrbk` (required for backup operations) and `util-linux` (required for `lsblk`); added both and fixed binary install path from `/usr/local/bin` to `/usr/bin`

## [0.7.5] - 2026-03-05

### Added
- **`snapshot_name` config field** — Subvolumes can now specify an explicit `snapshot_name` to override the algorithmic default, preventing collisions (e.g., `@` and `@root` both resolving to `root`)
- **`target_labels` config field** — Sources can now restrict which targets they back up to (e.g., HDD sources only to the 22TB primary, not the 2TB recovery drives)
- **Source volume auto-mount in Rust backup path** — `ensure_sources_mounted()` mounts top-level BTRFS volumes (`subvolid=5`) before calling btrbk; the shell script did this but the Rust CLI/GUI code path didn't
- **Optional dependencies in packaging** — `s-nail` (email reports) and `rsync` (ESP mirroring) declared as optional/recommended across all packaging formats (Arch, Debian, Fedora, Snap) and install guide

### Fixed
- **Backups producing "0 snapshots created, 0 sent"** — Three root causes fixed:
  1. btrbk.conf generated separate volume blocks per source×target instead of one per source with multiple inline targets
  2. Snapshot name collisions (`@` and `@root` both → `root`) caused btrbk errors
  3. Source top-level volumes not mounted before btrbk calls in Rust code path
- **btrbk.conf template rewrite** — `render_btrbk_conf()` now produces correct one-volume-block-per-source structure with inline targets, per-target retention overrides, and `resolve_snapshot_names()` collision detection
- **2TB target retention** — 2TB targets now get `7d` emergency recovery retention instead of the full `4w 12m 4y` deep retention meant for the 22TB drive

## [0.7.4] - 2026-03-05

### Added
- **`--force` flag for unattended setup** (`btrdasd setup --force`) — Non-interactive mode that skips all prompts and never removes or overwrites the backup database; enables scripted installs, upgrades, uninstalls, and full uninstalls without a TTY

### Fixed
- **btrbk.conf snapshot_dir hardcoded** — `render_btrbk_conf()` used hardcoded `.btrbk-snapshots` for all sources; HDD sources with custom `snapshot_dir` (e.g., `ClaudeCodeProjects/.btrbk-snapshots`) now generate correctly from per-source config
- **Production btrbk_conf path** — Config `btrbk_conf` pointed to old hand-written `/etc/btrbk/btrbk.conf` instead of the generated `/etc/das-backup/btrbk.conf`; backup commands were reading the wrong config
- **GUI table sorting missing** — SearchPanel, Health/Drives, and Health/Growth tables now have `QSortFilterProxyModel` with clickable column headers for sorting
- **Snapshot timeline sort order** — Added ascending/descending date sort toggle button to the SnapshotTimeline panel

## [0.7.3] - 2026-03-05

### Added
- **Growth trendline chart** — Health Dashboard growth tab now shows a Qt Charts line graph with per-target used-space trend and dashed capacity ceiling lines
- **Free and ETA columns** — Growth table now includes Free (total - used) and ETA Full (14-point linear regression projection of when disk fills)
- **Qt6 Charts dependency** — GUI now requires `qt6-charts` package for growth visualization
- **Distro package testing** — All packaging recipes (Arch, Debian, Fedora, Flatpak, Snap) are now build-tested on their respective distributions before release
- **KF6 Notifications and StatusNotifierItem** — Added missing KF6 dependencies to all packaging formats (required by GUI for desktop notifications and system tray)

### Changed
- **History "Sent" column** — Replaced wide "Bytes Sent" column (formatted byte sizes) with narrow binary "Sent" indicator: Yes (green icon) if data was sent, No (red icon) if backup failed, dash for dry-run/snapshot-only runs

### Fixed
- **Config version stuck at old value** — `setup --upgrade` now auto-updates the `version` field in `/etc/das-backup/config.toml` to match the installed binary version (was stuck at 0.6.0 through multiple releases)
- **Incremental indexing `snapshots_skipped` always 0** — `discover_snapshots()` filtered out already-indexed snapshots before returning, making `walk()` unable to count skipped snapshots; added `DiscoveryResult` struct with both new snapshots and total-on-disk count
- **Growth data missing total_bytes** — D-Bus helper growth JSON now includes `total_bytes` per entry (looked up from target health data) enabling Free/ETA calculations

## [0.7.2] - 2026-03-05

### Added
- **`--uninstall-all` mode** (`btrdasd setup --uninstall-all`) — Removes all installed files: generated configs (same as `--uninstall`), plus cmake-installed binaries, FFI library, D-Bus configs, polkit policy, systemd units, man page, shell completions, desktop entry, and icon
- **Auto-enable helper service** — `cmake --install` now runs `systemctl daemon-reload` and `systemctl enable btrdasd-helper.service` automatically

### Changed
- **GUI version from CMake** — `KAboutData` version in `gui/src/main.cpp` now uses `BTRDASD_VERSION` compile definition from `CMAKE_PROJECT_VERSION` instead of a hardcoded string; version stays in sync automatically across releases

### Fixed
- **GUI About dialog showed v0.6.0** — `KAboutData` had a hardcoded `"0.6.0"` version string that was never updated; now derived from CMake project version
- **Stale v0.6.0 binaries in `/usr/local/bin/`** — Manual install from earlier release left binaries in `/usr/local/bin/` that shadowed the cmake-installed `/usr/bin/` binaries due to PATH priority; removed and replaced with symlinks to canonical install locations
- **CMake ExternalProject stale build cache** — `cmake --build` didn't always rebuild Rust binaries when only `cargo build --release` had been run (different `--target-dir`); `build/cargo-target/` vs `indexer/target/release/` divergence caused installed binary to lag behind
- **Indexer UNIQUE constraint** — Resolved duplicate snapshot insertion errors during incremental indexing
- **bytes_sent measurement** — Added `statvfs(2)` disk usage delta measurement for btrbk v0.32 (which doesn't report transfer sizes)
- **7 interconnected GUI + backend bugs** — Resolved issues across D-Bus client, backup panel, health dashboard, and file browser
- **btrbk output parsing** — Corrected parsing of btrbk stdout for backup history recording
- **btrbk filter arguments** — Stopped passing target mount paths as btrbk filter arguments

## [0.7.1] - 2026-03-05

### Fixed
- **Installation instructions** — README and INSTALL.md "Recommended" install only ran `cargo build`, skipping GUI, D-Bus helper, FFI library, scripts, systemd units, polkit, and man page; changed to full `cmake` build path that installs all components by default
- **BUILD_FFI default** — INSTALL.md documented `BUILD_FFI` as `OFF` when CMakeLists.txt has it `ON`; corrected documentation
- **Module count** — Library has 13 public modules (not 12); `ffi` module was missing from counts in README, ARCHITECTURE.md, and CHANGELOG

## [0.7.0] - 2026-03-05

### Added
- **Source volume auto-mount** (`mount::ensure_sources_mounted`) — Mounts top-level BTRFS volumes (`subvolid=5`) before btrbk operations so snapshots can access `/@`, `/@opt`, `/@home` etc.; deduplicates shared volumes, creates snapshot dirs and target subdirs; returns `MountGuard` for RAII cleanup
- **Auto-mount/unmount** (`mount.rs`) — RAII `MountGuard` resolves target serials via `/dev/disk/by-id`, mounts BTRFS partitions before operations, unmounts on completion or panic; all D-Bus methods and CLI commands that access targets now auto-mount
- **D-Bus index read methods** — `IndexStats`, `IndexListSnapshots`, `IndexListFiles` (paginated), `IndexSearch`, `IndexBackupHistory`, `IndexSnapshotPath` for read-only index access from the GUI
- **Paginated `IndexListFiles`** — Accepts `limit`/`offset` parameters, returns JSON with `{files, total, limit, offset}` to handle snapshots with millions of files without D-Bus excess-data errors
- **`org.dasbackup.config.read` polkit action** — `allow_active: yes` for read-only config/schedule queries, prevents synchronous D-Bus deadlock when GUI requests config without admin auth dialog
- **`org.dasbackup.index.read` polkit action** — `allow_active: yes` for GUI read-only index access
- **USB SMART passthrough** — Health queries use `-d sat` for USB-attached drives to read SMART data through USB-SATA bridges
- **Growth log history in `HealthQuery`** — Parses `/var/lib/das-backup/growth.log` and includes growth history in health JSON response
- **Service status in `HealthQuery`** — Checks systemd timer/service status and includes in health JSON
- **`db::get_files_in_snapshot_paged()`** — Paginated file listing with `LIMIT`/`OFFSET` and `ORDER BY path`
- **`db::count_files_in_snapshot()`** — Efficient file count using `COUNT(DISTINCT f.id)` for pagination total
- **`FileModel::loadMore()`** — Incremental page loading in the GUI with `beginInsertRows`/`endInsertRows`

### Changed
- **Library modules** — 11 → 13 public modules (added `ffi`, `mount`)
- **Polkit policy** — 5 → 7 actions (added `config.read`, `index.read`)
- **D-Bus methods** — 17 → 23 (added 6 index read methods)
- **`ConfigGet`/`ScheduleGet` polkit** — Changed from `org.dasbackup.config` (auth_admin_keep) to `org.dasbackup.config.read` (allow_active) to prevent Qt event-loop deadlock
- **GUI architecture** — Removed direct `Database` class, rewired all models through `DBusClient`; `IndexRunner` converted from `QProcess` to D-Bus `IndexWalk`
- **Rust test count** — 62 → 161 (133 lib + 19 setup + 9 integration)

### Fixed
- **Source volumes not mounted for btrbk** — Full backup produced only 1 snapshot because `/.btrfs-nvme`, `/.btrfs-ssd`, `/.btrfs-hdd` were not mounted with `subvolid=5`; only `/dasRaid0` (pre-mounted) was accessible to btrbk
- **btrbk command construction** — `create_snapshots()` placed "snapshot" subcommand inside the source loop, producing `btrbk snapshot vol1 snapshot vol2` instead of `btrbk snapshot vol1 vol2`; fixed by moving `cmd.arg("snapshot")` before the loop
- **Volume deduplication** — Multiple sources sharing the same BTRFS volume (e.g., `hdd-projects` and `hdd-audiobooks` both on `/.btrfs-hdd`) caused duplicate btrbk arguments; fixed with `HashSet` deduplication in both `create_snapshots()` and `send_snapshots()`
- **Indexer UNIQUE constraint** — `INSERT INTO snapshots` failed on re-index when snapshot already existed; fixed with `INSERT OR IGNORE`
- **bytes_sent measurement** — Added `statvfs(2)` disk usage delta measurement since btrbk v0.32 doesn't report transfer sizes
- **BackupPanel TOML parser** — Removed `SourceEntry`/`SourceSubvol` struct handling that didn't match actual `config.toml` format; simplified to extract source/target labels only
- **Growth log ISO timestamp parser** — Fixed parsing of ISO 8601 timestamps in growth log
- **Multi-target re-index** — Fixed index walk to handle multiple targets correctly
- **JobProgress D-Bus signal** — Changed `percent` from `u8` to `i32` to match Qt D-Bus signal type
- **HealthQuery JSON key** — Changed GUI JSON key from `drives` to `targets` to match helper response

## [0.6.0] - 2026-02-28

### Added

#### Rust Library & CLI (Milestone 1)
- **`buttered_dasd` library crate** — Extracted 11 public modules from CLI binary into reusable library (`backup`, `config`, `db`, `health`, `indexer`, `progress`, `report`, `restore`, `scanner`, `schedule`, `subvol`)
- **`SubvolConfig` data model** — Replaced `Vec<String>` subvolumes with `Vec<SubvolConfig>` supporting `manual_only` flag (backward-compatible `#[serde(untagged)]` deserialization)
- **New CLI subcommands** — `backup` (run/snapshot/send/boot-archive/report), `restore` (file/snapshot/browse), `schedule` (show/set/enable/disable/next), `subvol` (list/add/remove/set-manual/set-auto), `health`, `config edit`, `completions`
- **`NewBackupRun` struct** — Structured input for backup run recording (replaces positional parameters)
- **Database tables** — `backup_runs` and `target_usage` tables for backup history and disk usage tracking
- **Shell completions** — `btrdasd completions <shell>` generates completions for bash, zsh, fish, elvish, and PowerShell via `clap_complete`
- **Man page** — `docs/btrdasd.1` with all subcommands, options, examples, and file paths

#### D-Bus Helper Daemon (Milestone 2)
- **`btrdasd-helper`** — Privileged D-Bus daemon on system bus (`org.dasbackup.Helper1`) with polkit authorization
- **D-Bus methods** — BackupRun, BackupSnapshot, BackupSend, BackupBootArchive, IndexWalk, RestoreFiles, RestoreSnapshot, ConfigGet, ConfigSet, ScheduleGet, ScheduleSet, ScheduleEnable, SubvolAdd, SubvolRemove, SubvolSetManual, HealthQuery, JobCancel
- **D-Bus signals** — JobProgress (stage/percent/message/throughput/ETA), JobLog (level/message), JobFinished (success/summary)
- **Job management** — Tokio-based async job execution with cancellation tokens and job ID tracking
- **Polkit policy** (`polkit/org.dasbackup.policy`) — 5 actions: backup, restore, config, index, health (expanded to 7 in [Unreleased])
- **D-Bus activation** (`dbus/org.dasbackup.Helper1.service`) — Automatic daemon startup on first method call
- **Bus access rules** (`dbus/org.dasbackup.Helper1.conf`) — System bus ownership and method access control

#### FFI Bridge (Milestone 3)
- **`libbuttered_dasd_ffi.so`** — C-ABI shared library (feature-gated `ffi` flag) for GUI access to Rust library
- **FFI functions** — Config load/get/validate/free, subvol list, health parse growth log, DB open/history/usage/free, format bytes, string free
- **C header** (`indexer/include/btrdasd_ffi.h`) — Opaque pointer types and function declarations
- **JSON interchange** — Complex data returned as JSON strings, parsed by GUI with `QJsonDocument`

#### GUI Infrastructure (Milestone 4)
- **Navigation sidebar** (`Sidebar`) — QTreeWidget with sections: Browse (Snapshots, Search), Backup (Run Now, History), Config, Health (Drives, Growth, Status)
- **D-Bus client** (`DBusClient`) — QDBusInterface wrapper with async method calls and signal connections for JobProgress/JobLog/JobFinished
- **Progress panel** (`ProgressPanel`) — Collapsible QDockWidget with progress bar, throughput, ETA, cancel button, and raw log viewer
- **Extended database** — `getBackupHistory()` and `getTargetUsageHistory()` methods with `BackupRunInfo` and `TargetUsageInfo` data structs

#### GUI Panels (Milestone 5)
- **Backup operations panel** (`BackupPanel`) — Mode selection (incremental/full), operation checkboxes (snapshot, send, boot archive, index, email), source/target selection, dry run support
- **Backup history view** (`BackupHistoryView`) — QTableView with timestamp, mode, duration, status, bytes sent, errors columns; auto-refresh on JobFinished
- **Health dashboard** (`HealthDashboard`) — Tabbed widget with Drives (QTableView from D-Bus), Growth (QChartView with QLineSeries per target), Status (btrbk/timer/mount status)
- **Config editor** (`ConfigDialog`) — KPageDialog with TOML editor, reload/diff/save toolbar, change confirmation dialog

#### Advanced GUI Features (Milestone 6)
- **Dolphin-style file browser** (`SnapshotBrowser`) — Breadcrumb navigation, switchable detail/icon views, QFileSystemModel, multi-select context menu (restore, copy path, properties), inline filter bar
- **First-run wizard** (`SetupWizard`) — QWizard with 5 pages: Welcome, Source Selection, Target Selection, Schedule, Summary; auto-launches when no config found
- **Desktop notifications** — KNotification on backup complete/fail with summary details
- **System tray** — KStatusNotifierItem with tooltip showing last backup status
- **Rich status bar** — "Next: Sun 04:00 | 3 targets online | DB: 2.1 GB | 42 snapshots" with 60-second auto-refresh
- **Keyboard shortcuts** — Ctrl+B (backup), Ctrl+R (restore), Ctrl+F (search), F5 (refresh)

### Changed
- **Crate architecture** — Split from CLI-only binary into library (`buttered_dasd`) + binary (`btrdasd`) + D-Bus helper (`btrdasd-helper`) + FFI cdylib with `[lib]`, `[[bin]]`, and feature flags in Cargo.toml
- **Regex performance** — `LazyLock<Regex>` for compile-once snapshot dirname parsing (replaces per-call `Regex::new()`)
- **Release profile** — Added `[profile.release]` with `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = true`
- **GUI architecture** — Refactored from flat splitter layout to sidebar + QStackedWidget central area (19 C++ components, up from 12)
- **CMake build system** — Added `BUILD_HELPER` and `BUILD_FFI` options alongside existing `BUILD_GUI` and `BUILD_INDEXER`
- **GUI dependencies** — Added Qt6::DBus, KF6::Notifications, KF6::StatusNotifierItem
- **XML GUI** — Version 4 → 5 with Backup and Tools menus, find_files action

### Fixed

## [0.5.1] - 2026-02-24

### Added
- **Full management interface design** — Architecture for transforming GUI from read-only browser into full backup management system with CLI parity
- **Design document** (`docs/plans/2026-02-24-full-management-interface-design.md`) — Complete architecture spec for v0.6.0
- **Implementation plan** (`docs/plans/2026-02-24-full-management-implementation-plan.md`) — 41-task phased plan across 5 phases

## [0.5.0] - 2026-02-22

### Added
- **Config-driven pipeline** (`btrdasd config dump-env`) — Reads `config.toml` and prints shell-sourceable `DAS_*` key=value pairs; scripts source config at runtime via `eval`
- **Config subcommands** — `btrdasd config dump-env`, `btrdasd config show`, `btrdasd config validate`
- **Extended config.toml schema** — New `[das]`, `[boot]` sections; per-source `snapshot_dir`; per-target `display_name`, `retention.daily`, `retention.yearly`
- **Hardware-agnostic documentation** — All docs describe the system generically; author's hardware moved to `docs/examples/` as reference examples
- **Planning worksheet** — Capacity estimation, drive selection, retention planning guide in `docs/OFFLINE-BACKUP-PLAN.md`
- **Generic bay mapping guide** — LED identification, serial mapping, config.toml integration in `docs/DAS-BAY-MAPPING.md`
- **Reference examples directory** — `docs/examples/` with author's bay mapping, storage topology, and index

### Changed
- **Scripts refactored** — `backup-run.sh`, `backup-verify.sh`, `boot-archive-cleanup.sh`, `das-partition-drives.sh` now use `eval "$(btrdasd config dump-env)"` instead of hardcoded values
- **Template engine** — Generated backup script replaced with thin `exec` wrapper; production scripts embedded via `include_str!` and copied during install
- **systemd units** — Use production paths (`/usr/local/lib/das-backup/`) and generic DAS detection instead of hardcoded dev paths
- **Documentation** — `STORAGE-ARCHITECTURE-AND-RECOVERY.md`, `DISASTER-RECOVERY-GUIDE.md`, `DAS-BAY-MAPPING.md`, `OFFLINE-BACKUP-PLAN.md` all parameterized with `<your-uuid>` placeholders

### Fixed
- **GUI restore action** — Implemented `Database::snapshotPathById()` and `m_currentSnapshotId` tracking; restore now correctly combines snapshot path with file path for `KIO::copy`

## [0.4.0] - 2026-02-21

### Added
- **KDE Plasma GUI** (`btrdasd-gui`) — Native Qt6/KF6 application for browsing and restoring backup files
  - 12 C++ components: MainWindow, Database, SnapshotModel, FileModel, SearchModel, SnapshotTimeline, IndexRunner, SnapshotWatcher, RestoreAction, SettingsDialog, desktop entry, XML GUI
  - Custom-painted timeline widget for visual snapshot navigation
  - FTS5 full-text search with debounced input
  - KIO-based file restore with destination chooser
  - QFileSystemWatcher auto-detection of new snapshots
  - KConfigDialog settings with database path, watch path, auto-watch toggle
  - 4 QTest suites (database, snapshotmodel, filemodel, searchmodel)
- **Interactive installer** (`btrdasd setup`) — 10-step dialoguer wizard with 5 modes:
  - `btrdasd setup` — Fresh install with interactive configuration
  - `btrdasd setup --modify` — Re-open wizard with existing config pre-filled
  - `btrdasd setup --upgrade` — Regenerate files from existing config after binary update
  - `btrdasd setup --uninstall` — Remove all generated files, optionally remove database
  - `btrdasd setup --check` — Validate config, verify files, check dependencies
  - System detection: block devices, BTRFS subvolumes, init system (systemd/sysvinit/OpenRC), package manager
  - Template engine: generates btrbk.conf, systemd/cron units, backup script, email config, ESP hooks
  - TOML-based configuration at `/etc/das-backup/config.toml`
- **Dockerfile** — Multi-stage build (rust:1.93-bookworm builder + debian:bookworm-slim runtime) for headless `btrdasd` CLI
- **CMake build options** — `BUILD_GUI` and `BUILD_INDEXER` toggles; `ExternalProject_Add` for Rust cargo build
- **Distro-agnostic init system support** — systemd, sysvinit, and OpenRC service/timer generation
- **docs/ARCHITECTURE.md** — Full system architecture with security and design decisions
- **docs/INSTALL.md** — Comprehensive installation guide for all 5 installer modes

### Changed
- **License**: GPL-3.0 → MIT
- CMake project version: 0.1.0 → 0.4.0
- systemd units now generated by installer from templates (no longer static files in `systemd/` directory)
- Rust minimum version: 1.85 → 1.87+ (edition 2024 `let_chains` feature)
- Indexer (`buttered-dasd` crate) version: 0.1.0 → 0.4.0
- GUI (`btrdasd-gui`) version: 0.1.0 → 0.4.0

### Fixed
- systemctl calls moved from `install_to_prefix` to `install` to prevent polkit authentication dialogs during test runs

## [0.3.0] - 2026-02-21

### Added
- **ButteredDASD content indexer** (`btrdasd`) — Rust CLI for indexing DAS backup snapshots
  - SQLite FTS5 full-text search across all indexed file paths and names
  - Span-based deduplication: unchanged files across consecutive snapshots stored as single row
  - Incremental indexing: only walks newly-created snapshots
  - 4 CLI subcommands: `walk` (index), `search` (FTS5), `list` (snapshot contents), `info` (stats)
  - WAL journal mode for concurrent read/write
  - Performance indexes on snapshots, files, and spans tables
  - 37 unit tests, zero clippy warnings, cargo audit clean
- Integrated `btrdasd` into `scripts/backup-run.sh` with soft-fail (indexing errors don't abort backup)
- Content indexer status line in email backup reports

### Changed
- Indexer built in Rust (edition 2024) instead of planned C++ for memory safety
- Application named ButteredDASD with CLI binary `btrdasd`
- Indexer binary path in backup-run.sh uses `BTRDASD_BIN` env var with `/usr/local/bin/btrdasd` default

## [0.2.0] - 2026-02-21

### Added
- Migrated backup scripts from CachyOS-Kernel project
  - `scripts/backup-run.sh` v3.1.0 — btrbk orchestrator with triple-target architecture, throughput logging, email reports
  - `scripts/backup-verify.sh` v2.0.0 — DAS drive health (SMART) + btrbk status verification
  - `scripts/das-partition-drives.sh` v1.0.0 — DAS drive partitioning with serial verification
  - `scripts/install-backup-timer.sh` — systemd timer installer (updated for new project structure)
  - `scripts/boot-archive-cleanup.sh` v1.0.0 — NEW: prune boot subvolume archives older than retention period
- Migrated btrbk reference config to `config/btrbk.conf`
- Created `config/das-backup-email.conf.example` — email config template (redacted credentials)
- Migrated systemd units to `systemd/` (paths updated for DAS-Backup-Manager)
  - `das-backup.service` + `das-backup.timer` — nightly incremental at 03:00
  - `das-backup-full.service` + `das-backup-full.timer` — weekly full on Sundays at 04:00
- Migrated documentation to `docs/`
  - `OFFLINE-BACKUP-PLAN.md` — capacity planning, drive allocation, backup strategy
  - `DISASTER-RECOVERY-GUIDE.md` — step-by-step recovery procedures
  - `STORAGE-ARCHITECTURE-AND-RECOVERY.md` — full system storage reference
  - `DAS-BAY-MAPPING.md` — physical drive locations and serial numbers
- CMakeLists.txt with install targets for scripts, config, and systemd units

## [0.1.0] - 2026-02-21

### Added
- Project scaffolding with CMake build system (ECM + Qt6 + KF6)
- GitHub repo with full security: Dependabot, CodeQL, secret scanning, branch protection
- GPL-3.0 license (changed to MIT in v0.4.0)

[Unreleased]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.12.1...HEAD
[0.7.12.1]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.12...v0.7.12.1
[0.7.12]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.11...v0.7.12
[0.7.11]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.10...v0.7.11
[0.7.10]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.9...v0.7.10
[0.7.9]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.8...v0.7.9
[0.7.8]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.7...v0.7.8
[0.7.7]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.6...v0.7.7
[0.7.6]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.5...v0.7.6
[0.7.5]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.4...v0.7.5
[0.7.4]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/TheBoscoClub/DAS-Backup-Manager/releases/tag/v0.1.0
