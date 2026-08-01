# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`btrdasd scrub` CLI subcommand** (task 3/6 of the scheduled-scrub feature, `bd DAS-Backup-Manager-0kn`): `Commands::Scrub` with `run`/`status`/`cancel` actions in `indexer/src/main.rs`, wired directly onto the engine's public API in `scrub.rs` (task 2/6) — no engine changes were needed
  - **`btrdasd scrub run`** invokes `scrub::run_scrub_pass()` — the exact locking/mount/scrub/report path the systemd timer will use, nothing extra added here. Runs even when `[scrub].enabled = false`, printing a clear `NOTE:` to stderr first: the enabled flag only gates the *scheduled* timer, never a manual invocation, and a disabled schedule is exactly when an operator wants to run one by hand. Exits 1 when any target failed
  - **`btrdasd scrub status`** reports per-filesystem age, outcome, error counters, bytes, and duration for every label in `[scrub].targets`, resolved by filesystem UUID via `resolve_target_fsuuid()` — never by mount path — so it works with the DAS drives unmounted. Two sources are consulted in order: the engine's `scrub-state.json` when an entry exists, falling back to the raw `/var/lib/btrfs/scrub.status.<fsuuid>` record for a filesystem whose scrub history predates this CLI (the real shape of all three DAS filesystems today — `scrub-state.json` doesn't exist yet, but all three have genuine finished records from earlier manual scrubs). A target with neither source renders as `NEVER SCRUBBED`, not an error — verified live as root against the production config: `primary-22tb` OK/68d/10.82 TiB, `system-recovery-A-2tb` OK/4d/1019.62 GiB (uuid `60b05268-…`, the 2026-07-27 finished record), `system-recovery-B-2tb` OK/106d/914.43 GiB, plus a synthetic never-scrubbed target rendering `NEVER SCRUBBED` cleanly
  - **`btrdasd scrub cancel`** is manual-operator-only — the interlock never auto-cancels. Probes `/run/das-scrub.lock` (`scrub::FileLock::try_acquire`, immediately released) to confirm a pass is live, then checks each configured target's mount point with `scrub::live_scrub_state()` to find the one actually scrubbing (safe against unmounted targets for the same reason the engine's own doc comment gives — only a filesystem the engine mounted for scrubbing can ever report `Running`) and issues `btrfs scrub cancel` against it. A clean no-op when nothing is running, verified live against the production `das-scrub.lock`
  - New unit tests in `main.rs` cover clap wiring (`Cli::command().debug_assert()`, all three actions parse), the never-scrubbed and btrfs-record-fallback status paths against isolated `DAS_SCRUB_STATE`/`DAS_BTRFS_STATUS_DIR` fixtures, an unresolvable-target error path, and a host-tolerant cancel smoke test
  - `docs/btrdasd.1` gained `scrub run`/`scrub status`/`scrub cancel` sections, examples, and `FILES` entries for `scrub-state.json` and the raw btrfs record path; incidentally corrected a stale `FILES` reference to the retired `/etc/das-backup-email.conf` (superseded 2026-05-16 per `.claude/rules/backup.md`) to the canonical `~/.config/pbridge.conf`
- **Scrub engine `indexer/src/scrub.rs`** (task 2/6 of the scheduled-scrub feature, `bd DAS-Backup-Manager-212`): new public module of the `buttered_dasd` library that owns a whole monthly pass — for each label in `[scrub].targets`, **in sequence** (the three DAS filesystems share one USB bus), it mounts by `UUID=<fsuuid>` at the target's configured mountpoint with `[das].mount_opts`, runs `btrfs scrub start -B`, parses `/var/lib/btrfs/scrub.status.<fsuuid>`, and unmounts. Two guarantees drive the design, both from the 2026-07-27 `system-recovery-A` investigation:
  - **`finished` is distinguished from `aborted`**: in the on-disk record an aborted scrub is only `canceled:0|finished:0` — there is no `aborted` key — so a parser that checks error counters alone passes it silently (as happened for 64 days). `ScrubStatusRecord::is_clean()` requires `ScrubOutcome::Finished` *and* zero damage counters; `canceled:1` wins over the `finished:1` btrfs sets alongside it; on a RAID-1 record the worst per-device outcome wins. `no_csum`/`csum_discards` are excluded from `error_total` (a nodatacow filesystem legitimately reports millions), `corrected_errors` is included
  - **Every resolution is by filesystem UUID, never by mount path**: `btrfs scrub status <path>` on an unmounted path silently returns the record of whatever filesystem backs it. `status_record_path()`/`read_scrub_status()` accept only a UUID, `parse_scrub_status()` rejects a record whose rows carry a different UUID, `resolve_target_fsuuid()` reads `[[target]].mount_uuid` or falls back to serial → partition → `blkid`, and every mount is verified with `findmnt -o UUID` before a scrub starts against it. A record whose `t_start` predates *this filesystem's* scrub start is reported stale rather than mistaken for a result
- **Guarded `btrfs scrub start -f` so an aborted record cannot wedge a filesystem's scrubs** — `scrub::decide_scrub_start_mode()` adds `-f` when, and only when, the prior record for that FS UUID is `Aborted` (`canceled:0 finished:0`) **and** `live_scrub_state()` confirms the kernel has no scrub in progress. `btrfs-progs` can read such a record as evidence of a running scrub and refuse to start — the documented remedy for exactly that case (`man btrfs-scrub`, `-f`: "useful when scrub status file is damaged and reports a running scrub although it is not"). Force is never unconditional: these locks cannot stop an operator starting a scrub by hand, and force-restarting theirs would discard hours of work, so a live scrub (or any inability to determine liveness) falls back to a plain start that fails loudly. Liveness comes from `btrfs scrub status`'s `Status:` line, the only kernel-truth signal available — the `scrub.progress.<uuid>` socket was empirically ruled out, as background scrubs do not create one
- **Root-gated loopback integration tests `indexer/tests/scrub_loopback.rs`** — two `#[ignore]`d end-to-end tests over a real BTRFS filesystem on a loop device: `loopback_full_pass_then_forced_recovery_from_aborted_record` exercises mount-by-UUID → `findmnt` verification → real `btrfs scrub start -B` → record parse → state persist → unmount, then repeats it with a forged aborted record and asserts the forced scrub is genuinely fresh (new `t_start`, full byte count re-verified); `live_scrub_is_never_force_restarted` puts a real scrub on a `dm-delay`-throttled device, asserts the engine chooses `Normal` even with an aborted record present, and confirms btrfs itself refuses the concurrent start. A `Drop` guard tears down mounts, dm targets, loop devices, images, and the FS's `/var/lib/btrfs` record even on panic
- **Two-lock design in the scrub engine** — non-blocking `flock` on `/run/das-scrub.lock` (held ⇒ log and skip, never queue, no state written) then blocking `flock` on `/run/das-maintenance.lock` (shared with `backup-run.sh`), held for the entire pass including the engine's own mounts and unmounts; contention defers, never cancels, and logs `waiting for DAS maintenance lock (backup in progress?)` once the wait exceeds 5 s. `ScrubLocks` field order makes the release order the exact reverse of acquisition. Locks live in the engine, not the systemd unit, so a manual run is covered identically; `/run` is tmpfs so nothing goes stale across a boot
- **Scrub state file `/var/lib/das-backup/scrub-state.json`** — schema v1, written atomically (temp + rename) at mode `0644` so health checks read it **without any DAS filesystem mounted**. Keyed by filesystem UUID; each entry carries `target_label`, `mountpoint`, `last_attempt` (outcome, `ok`, timestamps, duration, bytes, per-counter breakdown, engine messages) and `last_success_epoch`, which is carried forward across failed passes and is the field to age against `warn_age_days`/`fail_age_days`. Full schema documented in the module docs. A skipped pass writes nothing; an unparseable file is quarantined to `scrub-state.json.corrupt` and rewritten, while an intact-but-unreadable file aborts the write rather than erase the success history. `load_state_from()` is **pure** — it never renames or deletes, so the unprivileged readers (health checks, GUI) cannot mutate the writer's state and get an error that means exactly one thing; quarantine is the writer-side `quarantine_state_file()`. The atomic write also fsyncs the parent directory, so the rename itself survives a crash
- **Scrub email report** — one concise email per completed pass via the existing pipeline: `report.rs` gained `send_email_report_with_kind()` (the pre-existing `send_email_report()` is now a thin wrapper passing `Backup`), so scrub mail arrives as `[DAS Scrub] <host> — SUCCESS|FAILURE — <time>` and stays visually distinct from backup mail. Credentials still come from the single canonical `/home/bosco/.config/pbridge.conf`
- **`[scrub]` config section for scheduled BTRFS scrub** (task 1/6 of the scheduled-scrub feature, `bd DAS-Backup-Manager-ikn`): new `Scrub` struct in the Config model (`indexer/src/config.rs`) and generated `/etc/das-backup/config.toml`. Fields: `enabled` (bool, default `true`), `on_calendar` (systemd `OnCalendar=` string consumed verbatim by a later timer template, default `"*-*-01 05:30:00"` — monthly, 1st, 05:30), `targets` (ordered list of config target labels — i.e. `[[target]].label` values, not BTRFS filesystem labels — scrubbed sequentially, default `["primary-22tb", "system-recovery-A-2tb", "system-recovery-B-2tb"]` — UUIDs resolve from the existing `[[target]]` blocks at use time by joining on this field, never duplicated), and health thresholds `warn_age_days` (default `45`) / `fail_age_days` (default `75`), both consumed by a later health-check task. Exported to `btrdasd config dump-env` as `DAS_SCRUB_ENABLED`/`DAS_SCRUB_ON_CALENDAR`/`DAS_SCRUB_TARGETS`/`DAS_SCRUB_WARN_AGE_DAYS`/`DAS_SCRUB_FAIL_AGE_DAYS` (`indexer/src/setup/env_export.rs`). Existing configs without `[scrub]` parse to these defaults via `#[serde(default)]` — no breaking change. No new wizard prompts added — mirrors the `[das]`/`[boot]` sections, which are also default-only and untouched by `setup/wizard.rs`. Design decisions (unbounded monthly pass, sequential, per-filesystem — three scrubs cover four DAS drives since the 22tb target is one RAID-1 filesystem) were user-confirmed 2026-07-27. Verified live: `sudo btrdasd setup --upgrade` regenerated `/etc/das-backup/config.toml` with the `[scrub]` section populated from defaults, and `btrdasd config dump-env` emits the new `DAS_SCRUB_*` vars
- **Boot archive step in the bash backup orchestrator** — `scripts/backup-run.sh` `update_boot_subvolumes()` (v4.2.3 → v4.2.4) now archives the outgoing `@`/`@home` as a read-only snapshot BEFORE the existing create-then-swap replacement on `--full` runs, matching `indexer/src/backup.rs` `archive_boot()` semantics exactly (the Rust manual path's archive-then-recreate behavior is authoritative per a 2026-08-01 user decision). Archive names are `@.archive.<TS>` / `@home.archive.<TS>` with `TS=$(date +%Y%m%dT%H%M%S)`, one timestamp per run, matching the format `boot-archive-cleanup.sh` already parses. If the archive snapshot fails, the recreation is skipped for that subvolume — the only copy of the outgoing `@`/`@home` is never destroyed. Previously the bash path recreated `@`/`@home` with no archive step at all. Tracks `bd DAS-Backup-Manager-1j7`
- **Boot archive pruner wiring** — `scripts/boot-archive-cleanup.sh` was installed to `/usr/lib/das-backup/boot-archive-cleanup.sh` but nothing invoked it, leaving `archive_retention_days` an unenforced policy. `backup-run.sh` now runs it via a new `run_archive_cleanup()` function at the end of every run — daily and full alike — while targets are still mounted, since the pruner silently skips any target mount point that isn't currently mounted. Wrapped in `record_op "archive_cleanup"` so status/detail surface in `generate_report()`'s BACKUP OPERATIONS section and the email report. Soft-fails like the content indexer — a pruner failure is recorded but never aborts the backup. When `backup-run.sh` itself runs `--dryrun`, `--dryrun` is passed through to the pruner rather than skipping it. Tracks `bd DAS-Backup-Manager-64h`
- **`.claude/rules/backup.md` — "Delete vs mutate — send/receive chain safety" subsection**: documents the invariants that keep a btrbk send/receive chain intact — deleting a target snapshot is safe while at least one common pair with the source survives, never clear `ro` on a received subvolume (permanently destroys the Received UUID), mutating an already-sent local parent silently desyncs source and target because `btrfs send -p` never re-emits what the target is missing, and in-place purging is safe only for snapshots that are neither send sources nor received
- **Install-time zero-byte binary validation in `CMakeLists.txt`** — new `das_require_nonzero_artifact()` function emits an `install(CODE …)` block that aborts `cmake --install` with a `FATAL_ERROR` if any Rust artifact in `${CARGO_TARGET_DIR}/release/` is missing or zero bytes. Called before each of the three Rust install rules (`btrdasd`, `btrdasd-helper`, `libbuttered_dasd.so`). Three separate runtime install regressions (May 2026) had `cmake --install` faithfully copy 0-byte stubs over working binaries in `/usr`, leaving services that failed with `203/EXEC` (Exec format error) only detectable via a failed scheduled job or a runtime D-Bus dialog. Defense-in-depth tracked by `bd DAS-Backup-Manager-9rx`; complements ongoing investigation of the underlying CMake/cargo interaction in `bd DAS-Backup-Manager-29j`. Verified by truncating `build/cargo-target/release/btrdasd-helper` to 0 bytes and confirming `cmake --install` aborts before touching `/usr`
- **Second 22TB CMR drive in DAS bay 5; `das-backup-22tb` converted from single-device to BTRFS RAID-1** — Seagate Exos `ST22000NM000C-3WC103` serial `ZXA1NYGZ` (~38h SMART extended self-test passed prior to commit). Partitioned identically to existing leg (`d3ac162f-…`): GPT type `8300`, sectors 2048–42970644446, partition name `das-backup-22tb`, PARTUUID `b24e0ea8-fd90-4a36-8c76-26587a29755b`. Added to filesystem UUID `46ffbd7c-dfd9-4ba5-82ae-0afffde99bb1` as devid 2; `btrfs balance start -dconvert=raid1 -mconvert=raid1 -sconvert=raid1` rewrote all data, metadata, and system chunks across both legs. Sourced from a different manufacturing batch than `ZXA0LMAE` to mitigate correlated-failure risk.
- **DISASTER-RECOVERY-GUIDE.md Scenario D — 22TB RAID-1 Backup Array Single-Leg Failure** — step-by-step procedure for confirming the failure, mounting the surviving leg degraded, sourcing/SMART-checking a replacement, partitioning to match, `btrfs replace`-ing the failed devid, restoring RAID-1 across single-profile chunks written while degraded, and verifying integrity. Also added the layman walkthrough to `EMERGENCY-QUICK-REFERENCE.md` so the printable cheat sheet covers it.

### Changed
- **`[das].mount_opts` in `/etc/das-backup/config.toml` now includes `degraded`** — so a single-leg failure of the new 22TB RAID-1 array does not block backups, restores, or recovery. Trade-off: any chunks written while degraded are allocated as `single` profile until a post-replacement balance restores RAID-1 (documented in the new Scenario D)
- **DAS bay map (2026-05-06 reshuffle)** — bay 3 emptied, `ZFL41DNY` (das-backup-system, 1.8 TB SMR) moved from bay 3 → bay 4 to free bay 5 for the new 22TB drive. Updated `display_name` fields in `config.toml` to reflect the new bay numbers and the RAID-1 pairing: `primary-22tb` is now `"22TB Exos RAID-1 (Bays 2+5)"`, `system-2tb` is `"2TB System (Bay 4)"`. Re-ran `btrdasd setup --upgrade` so derived files (`/etc/btrbk/btrbk.conf`, generated scripts, systemd units) pick up the new env exports
- **`docs/examples/author-bay-mapping.md`, `docs/examples/author-storage-reference.md`, `docs/OFFLINE-BACKUP-PLAN.md`** — refreshed bay grid, drive details, role summary, and the "Why RAID-1 on the Primary Backup" rationale block. Now includes PARTUUID + UUID_SUB per leg so a failed-leg recovery can match identifiers exactly
- **`.claude/rules/backup.md`** — replaced stale entries (wrong DAS model, wrong device letters, wrong retention numbers, RAID-0 reference for the 22TB target) with the current state from `config.toml` and the new RAID-1 layout
- **`.claude/rules/esp-safety.md`** — `ZFL41DNY` now annotated as bay 4 (was bay 3 prior to 2026-05-06); `ZXA1NYGZ` added to the recognized-drive list as the new RAID-1 partner
- **Boot archive retention default lowered from 365 to 60 days** — `indexer/src/config.rs` `default_archive_retention_days()` and the `Boot::default()` impl, plus the `roundtrip_default_config` and `backward_compat_old_config_without_new_fields` unit tests that asserted the old default. Live `/etc/das-backup/config.toml` `archive_retention_days` updated `365` → `60` (previous file backed up to `config.toml.bak.20260801-141312-pre-retention-60d` first). `scripts/boot-archive-cleanup.sh` header comment and `.claude/rules/backup.md` retention lines updated to match; `boot-archive-cleanup.sh` bumped `v2.0.0` → `v2.0.1` since its behavior-claim header comment changed

### Fixed
- **`btrdasd-helper` `IndexStats`/`Index*` D-Bus methods no longer block the async runtime on multi-GB indices** — all six `index_*` read methods (`index_stats`, `index_list_snapshots`, `index_list_files`, `index_search`, `index_backup_history`, `index_snapshot_path`) now wrap their sync SQLite work in `tokio::task::spawn_blocking`, matching the pattern already used by `backup_*` and `health_query`. Previously they ran SQLite I/O directly on the runtime worker thread; under memory pressure on a 10 GB index this serialized concurrent calls and caused the GUI's 25 s D-Bus call deadline to expire (visible as `IndexStats: Did not receive a reply` modal in the GUI Health Dashboard). Additionally `Database::open` now sets `PRAGMA mmap_size = 4 GiB` and `PRAGMA cache_size = -262144` (256 MiB per connection) so SQLite reads via the shared kernel mmap rather than per-call `read()` syscalls. The helper also gains a per-DB-path `StatsCacheEntry` cache keyed by `(mtime, size)` — the cold-cache `COUNT(*) FROM files` on a 13.7M-row table is ~10 s alone and `COUNT(*) FROM spans` on 68M rows is even longer, so caching the result and only recomputing after the indexer bumps the DB mtime turns subsequent `IndexStats` calls into ~50 ms responses (verified with `drop_caches` to confirm the cached value is served from RAM, not the page cache). Finally a startup pre-warm task fires `IndexStats` once at helper boot so the first GUI click hits a populated cache. Tracks `bd DAS-Backup-Manager-aem`. Verified end-to-end: helper restart → 25 s background pre-warm → cold `gdbus` IndexStats returns in 44 ms with kernel cache dropped
- **`backup-run.sh create_snapshot_dirs` no longer silently creates `.btrbk-snapshots` directories under `/` instead of the source volume** — v4.2.2 → v4.2.3. Each `[[source]]` in `config.toml` declares `snapshot_dir` as a **path relative to the source's `volume`** (e.g. `.btrbk-snapshots` for `volume = "/.btrfs-hdd"`). The previous loop did `mkdir -p "${SOURCE_SNAPSHOT_DIRS[$label]}"` against the script's cwd, which under systemd is `/` — so the dir was created at `/.btrbk-snapshots` instead of `/.btrfs-hdd/.btrbk-snapshots`. Pre-existing snapshot dirs in the older volumes (`/.btrfs-nvme/.btrbk-snapshots`, `/.btrfs-ssd/.btrbk-snapshots`, `/.btrfs-hdd/ClaudeCodeProjects/.btrbk-snapshots`) masked the bug for months because they had been hand-created at some point in the past. When commit `06f4ab0` added the `hdd-media` and `hdd-system` sources on 2026-05-17, the missing `/.btrfs-hdd/.btrbk-snapshots/` caused btrbk to emit `WARNING: Skipping subvolume X: Failed to fetch subvolume detail for snapshot_dir` for all 4 new subvolumes and exit 10 — 7 consecutive nightly runs reported `Status: FAILURES DETECTED` between 2026-05-17 and 2026-05-23. `create_snapshot_dirs` now composes `${SOURCE_VOLUMES[label]}/${SOURCE_SNAPSHOT_DIRS[label]}` before `mkdir`, treats absolute snapshot_dir values as-is, warns and skips when the source volume is unknown, surfaces mkdir failures, and logs each resolved path. Stray empty dirs at `/.btrbk-snapshots`, `/ClaudeCodeProjects/.btrbk-snapshots`, `/Audiobooks/.btrbk-snapshots` (created by the buggy version) were rmdir'd as part of the fix. Tracks `bd DAS-Backup-Manager-0p2`. Verified by an isolated test harness (`tests/test_create_snapshot_dirs.sh`, 6/6 assertions pass) plus a live manual run that observed all 4 source-side snapshots land in `/.btrfs-hdd/.btrbk-snapshots/`
- **`backup-run.sh` no longer falls through to writing on `/` when a DAS target is missing or its mount fails** — v4.2.1 → v4.2.2. Two layers added:
  - `create_mount_points` (mountpoint hygiene): only `mkdir`'s `$mnt` for targets where `TARGET_AVAILABLE[label]="true"`. For unavailable targets it `rmdir`'s any pre-existing empty bare directory so `btrbk`'s configured path no longer exists at write time; if the bare directory is non-empty (i.e. evidence of a prior leak), the script aborts with a fatal error pointing to the recovery procedure
  - `verify_targets_before_btrbk` (post-mount assertion, new function): runs after `mount_targets` and `create_target_dirs`, before `run_btrbk`. For each configured target it verifies either (a) the path is a real `mountpoint -q` AND `findmnt -o UUID` (or device serial via `smartctl`) matches the value expected in `config.toml`, or (b) the path does not exist on disk. Any violation aborts the run with a clear per-target error listing
  - Caused a real incident in May 2026 when the original 22TB drive was removed and `btrbk` faithfully wrote backup subvolumes to `/mnt/backup-22tb` as a bare directory on `/` (NVMe RAID-1), filling the root filesystem and forcing urgent cleanup. The new guards refuse to invoke `btrbk` in that state. See `bd DAS-Backup-Manager-9on`. Verified: 4-test unit harness covering both empty-bare-dir cleanup, non-empty-bare-dir abort, mount-failed abort, and unavailable-with-leftover-dir abort, plus live happy-path run with 3 mounted targets (`OK (/mnt/backup-22tb → UUID=…)`, `OK (/mnt/backup-system → /dev/sdk2, serial=ZFL41DNY)`, etc.). Because `scripts/backup-run.sh` is embedded into `btrdasd` via `include_str!` at compile time, `cmake --install` deploys both `/usr/lib/das-backup/backup-run.sh` and the updated `/usr/bin/btrdasd` together
- **GUI's six-dialog startup cascade collapsed to a single notification when `btrdasd-helper` is unreachable** — when the helper cannot be activated on the system D-Bus (0-byte install, masked unit, polkit refused, etc.), `DBusClient` now probes once via `org.freedesktop.DBus.Peer.Ping` from the constructor, sets `m_available = false`, schedules a deferred `helperUnavailable(reason)` signal via `QTimer::singleShot(0, …)`, and skips D-Bus signal subscriptions. Every public method (`configGet`, `scheduleGet`, `healthQuery`, `indexStats`, `indexListSnapshots`, `indexBackupHistory`, `indexListFiles`, `indexSearch`, `indexSnapshotPath`, every async variant, every job-starting call routed through `callAsync`, and every config/subvol mutator) gains an `if (!m_available) return …;` guard so cold-start view calls short-circuit without firing per-method `errorOccurred` dialogs. `MainWindow` connects to the new signal and shows one `KMessageBox::error` with `systemctl status btrdasd-helper.service` and `journalctl -u btrdasd-helper.service -n 30` as recovery guidance. Tracked in `bd DAS-Backup-Manager-mw0`. Verified by truncating `/usr/libexec/btrdasd-helper` to 0 bytes and counting D-Bus activation attempts via `journalctl`: the cascade drops from 6+ attempts to 2 (one `QDBusInterface` auto-introspect at construction + one explicit `Peer.Ping`), and the modal dialog count drops from 6 to 1

## [0.7.12.3] - 2026-05-04

### Changed
- **Cargo dependency bumps via Dependabot** — `clap` 4.6.0→4.6.1, `clap_complete` 4.6.0→4.6.3, `libc` 0.2.184→0.2.186, `zbus` 5.14.0→5.15.0, `tokio` 1.51.0→1.52.1. All patch/minor; no behavioral changes; 136 tests pass on the new lockfile (PR #22)
- **GitHub Actions runner bumps via Dependabot** — `softprops/action-gh-release` 2.6.1→3.0.0 and `actions/upload-artifact` 7.0.0→7.0.1, both moving to Node 24. Action interfaces unchanged (PRs #18, #19)
- **`dependabot-auto-merge.yml` workflow repaired** — the workflow had pinned a non-existent SHA for `dependabot/fetch-metadata` (the comment said `v2.4.0` but the SHA below was a copy-paste mistake), causing every Dependabot PR's auto-merge job to fail since the pin landed. Bumped to `v3.1.0` with verified SHA so future Dependabot PRs auto-merge cleanly

### Fixed
- **`das-backup.service` exited 1 after every successful btrbk replication** — `update_boot_subvolumes()` in `scripts/backup-run.sh` ran two pipeline assignments per target (`latest_root=$(btrfs subvolume list "$mnt" | grep "nvme/root\." | awk … | sort | tail -1)` and the matching `latest_home`). On mirror targets the host only replicates `nvme/home.*`, never `nvme/root.*`, so `grep` returned 1, `set -o pipefail` propagated it, and `set -e` killed the script before the empty-string check on the next line could run. The failure happened silently on iter 2 of the loop, which is why the last log line was always iter 1's `@home exists, skipping` — making the symptom look like the skip itself was fatal. Fix: append `|| true` to both pipeline assignments so the empty-result branch (`[[ -z "$latest_root" || -z "$latest_home" ]]`) is reachable as originally intended. The mirror-skip guard added in `7f334a7` already covers this on a second axis once the rebuilt `btrdasd` binary is deployed (the script is embedded into the binary via `include_str!` at compile time, so a source-only update without rebuild has no effect)

## [0.7.12.2] - 2026-04-11

### Fixed
- **Debian `Build .deb` step failed with `Bad substitution`** — the step ran under `sh -e` (the default GitHub Actions shell), and the bash-specific substitution `${VERSION_NUM//-/\~}` used to sanitize `-` → `~` in the injected debian/changelog version is not supported by dash (Debian trixie's `/bin/sh`). Arch and Fedora jobs happened to work because both containers ship bash as `/bin/sh`. Fix: explicit `shell: bash` on the Debian Build .deb step so the substitution runs under bash regardless of container default
- **Arch `PKGBUILD` was missing `hicolor-icon-theme` dependency** — the package installs an icon under `/usr/share/icons/hicolor/scalable/apps/btrdasd-gui.svg` but did not declare the hicolor theme hierarchy as a dependency. namcap flagged this as an error during a local build audit. Fix: added `hicolor-icon-theme` to the `depends=` array
- **Arch `PKGBUILD` stale `pkgver=0.7.10`** — the committed PKGBUILD had not been bumped since 0.7.10, seven releases ago. The release-packages CI workflow already sed-patches `pkgver` at build time from `$VERSION_TAG` so this was cosmetic only, but the file-level version is now in sync with the rest of the packaging manifests

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

[Unreleased]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.12.3...HEAD
[0.7.12.3]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.12.2...v0.7.12.3
[0.7.12.2]: https://github.com/TheBoscoClub/DAS-Backup-Manager/compare/v0.7.12.1...v0.7.12.2
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
