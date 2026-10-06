# GUI Operations and the Boot Step — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every operation the GUI offers (Snapshot, Send, Boot Archive, Index, Email Report) is
sent to the helper and honoured by the library, and the boot-subvolume step behaves and reports
identically on the Rust path and in `backup-run.sh`.

**Architecture:** One D-Bus call (`BackupRun`) carries a steps dictionary into the one backup job
(`backup::run_backup_job`), so every GUI run gets locks, sync, mount verification, the report, the
history row and the 0/3/1 outcome. The three half-selection D-Bus calls (`BackupSnapshot`,
`BackupSend`, `BackupBootArchive`) are removed. The boot step gets one FAIL/WARN/skip
classification, written once in `.claude/rules/backup.md` and implemented in both
`backup::archive_boot_with` and `update_boot_subvolumes`. The script takes its boot plan (which
subvolumes, which snapshot names, which subdirectories) from the Rust library through a new
`btrdasd backup boot-plan` command, so the names are derived in exactly one place.

**Tech Stack:** Rust 2024 (`buttered_dasd`, `btrdasd`, `btrdasd-helper` on zbus), C++20 / Qt 6 / KF6
(`btrdasd-gui`), bash (`scripts/backup-run.sh`), CMake/ctest.

**Spec:** the decisions recorded on bd — `DAS-Backup-Manager-c4x` (notes 2026-10-05 19:09, 21:26,
2026-10-06), `-woq` (notes 2026-10-05 19:09, 2026-10-06), `-dtm` (description, note 2026-10-02,
note 2026-10-06), `-hyvh`, `-8veh`. Read all five with `bd show` before Task 1. The 2026-10-06
note on each supersedes earlier wording where they differ.

## Global Constraints

- Build only through CMake into tmpfs: `cmake -S . -B /tmp/das-c4x-build -DCMAKE_BUILD_TYPE=Release && cmake --build /tmp/das-c4x-build`. Never a bare `cargo build`.
- Rust tests: `cd indexer && CARGO_TARGET_DIR=/tmp/das-c4x-target cargo test` and again with `--features dbus`.
- Before every commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` (both feature sets), `shellcheck scripts/*.sh tests/*.sh`, ctest with `-DREQUIRE_ALL_SHELL_CASES=ON`.
- C++: `-Wall -Wextra -Wpedantic -Werror` (already set); new-style `connect`; every interactive widget has a tooltip.
- Bash in `scripts/`: never `local x=$(cmd)`; never pipe a listing into an early-exit reader (`grep -q`, `head`); no `[0-9]`-style bracket ranges in `=~` (the 1bsx lint) — use `[[:digit:]]`; compare strings bytewise under `local LC_ALL=C`.
- Fail-silent law (`.claude/rules/fail-silent.md`): a missing reading is never 0 or "no"; a refused input is never defaulted; an accepted-and-ignored parameter is a defect.
- A selection of nothing is a refusal, never "all" (bd 7tx) — this extends to Snapshot/Send.
- Mutation gate on the diff: copy the tree, `env -u CARGO_TARGET_DIR cargo mutants --in-place --no-shuffle --in-diff <crate-relative diff>`, then `.github/scripts/mutants-gate.py` must print `mutants OK`. Header of `indexer/.cargo/mutants.toml` has the exact recipe.
- Commits: signed, heredoc message, NO trailer and NO AI attribution of any kind; `~/.claude/bin/scrub-promo check origin/main..HEAD` must print CLEAN before any push.
- Versions: project `0.7.22.3` → `0.7.23.0` (`CMakeLists.txt` `project(VERSION …)`, `indexer/Cargo.toml`, every packaging file); `backup-run.sh` `4.11.3` → `4.12.0`.
- Never `cmake --install` while a das-* backup unit runs or `/run/das-backup.lock` is held (`backup.md` §Never Run…).

## Review Focus

1. **A GUI and a helper from different builds.** An old GUI calling the new `BackupRun` (4 args) or a new GUI calling an old helper must fail loudly (zbus `InvalidArgs`/signature error shown in the GUI), never run with defaults. Pinned by Task 1's "missing key is refused" test and Task 6's signature test.
2. **`[boot] enabled = false` while the GUI box is ticked** (a config edited after the GUI loaded). Expected: no `btrfs` call, report row `OK (disabled in config)`, run green. Pinned in Task 4.
3. **A primary target whose subvolume listing is larger than a pipe (> 64 KiB)** in the script. Expected: the newest snapshot is still found — no SIGPIPE, no temp file, no "none found". Pinned in Task 5 with the existing `drift-big` fixture shape.
4. **Snapshot names containing pattern characters** (`root-`, a `.` or `*` in a name). Expected: matched literally on both paths. Pinned by the shared fixture (Task 3) used by both test suites.
5. **An unknown or empty `mode` string over D-Bus.** Expected: refused (`InvalidArgs`), never read as Incremental. Pinned in Task 6.

## Decisions taken in this plan (for the operator to confirm at review)

- **"Snapshot + Send" keeps each mode's existing pipeline**: Full = one `btrbk run`; Incremental = `btrbk snapshot` then `btrbk resume` (both enforce retention). The c4x note's shorthand "Snapshot+Send = btrbk run" is read this way; Snapshot only = `btrbk snapshot`, Send only = `btrbk resume`, in both modes.
- **The GUI's Boot Archive box is available in both modes** (the 2026-10-06 decision — incremental creates a missing boot subvolume — supersedes woq's "unticked and disabled for Incremental"). Default ticked; its tooltip says what it does in the selected mode. Unticked and disabled, with the reason, when `[boot] enabled = false`.
- **`BackupSnapshot`, `BackupSend`, `BackupBootArchive` are removed** from the helper and `DBusClient` (rbp M9: no callers; `BackupSend` widened to every source).
- **The report is always written; the Email tick governs mailing only** (`backup.md` §Email Reports: the report is written before any send).

## The boot-step classification (both paths — Task 4 writes it into `backup.md`)

| Situation (per target, per configured boot subvolume) | Outcome |
|---|---|
| `[boot] enabled = false` | step not run; row `OK (disabled in config)` |
| GUI Boot Archive unticked | step not run; row `N/A (not selected)` (Rust only) |
| Target not selected / not mounted (absent mount point) | not counted, Info |
| Mirror target | skipped (counted once per target), Info |
| Mount state cannot be told / write verification refuses | **FAIL** (whole step) |
| `btrbk.conf` cannot be read (no boot plan) | **FAIL** (whole step) |
| Target's subvolume listing cannot be read | **FAIL** (once per target) |
| A boot subvolume with no `snapshot_name` in `btrbk.conf` | **WARN** |
| No source declares `target_subdirs` for it | **WARN** |
| No snapshot of that series on the target | **WARN** |
| Incremental, the subvolume exists on the target | skipped, Info |
| The subvolume is absent (either mode) | create from the newest snapshot; failure **FAIL** |
| Full, it exists: archive `-r` fails | **FAIL**, live untouched |
| Full: stale `<subvol>.new` cannot be removed | **FAIL**, live untouched |
| Full: building `<subvol>.new` fails | **FAIL**, live untouched |
| Full: deleting the live one fails | **FAIL**, staging discarded |
| Full: the rename fails | **FAIL** (archive holds the old) |
| `btrfs` cannot be run at all | **FAIL** |

Status: any FAIL → `FAIL (<u> updated, <f> failed)`; else any WARN → `WARN (<u> updated, <s> skipped, <w> warnings)`; else `OK (<u> updated, <s> skipped)`. FAIL fails the run (exit 3); WARN does not (exit 0, report `COMPLETED WITH WARNINGS`).

Snapshot match rule (both paths): a listing line's last field equals `<subdir>/<snapshot_name>.<TS>`
with `TS` = 8 ASCII digits, `T`, 4 ASCII digits, optionally `_` and digits; `subdir` has leading and
trailing `/` trimmed; the newest is the bytewise-greatest matching path.

---

### Task 1: Step selection in the library — `BtrbkSteps` and `RunSteps`

**Files:**
- Modify: `indexer/src/backup.rs` (`BackupOptions` :123-153, `run_pipeline` :1739-1802, `run_backup_with` :1939-2005)
- Test: `indexer/src/backup.rs` `mod tests`

**Interfaces:**
- Produces: `pub enum BtrbkSteps { SnapshotAndSend, SnapshotOnly, SendOnly }` (derive `Debug, Clone, Copy, PartialEq, Eq, Default`, default `SnapshotAndSend`), `BtrbkSteps::from_ticks(snapshot: bool, send: bool) -> Result<BtrbkSteps, String>`, `.snapshots() -> bool`, `.sends() -> bool`; `pub struct RunSteps { pub btrbk: BtrbkSteps, pub boot_archive: bool, pub index: bool, pub email: bool }`, `pub const RUN_STEP_KEYS: [&str; 5] = ["snapshot", "send", "boot_archive", "index", "email"]`, `RunSteps::from_entries<I: IntoIterator<Item = (String, Option<bool>)>>(entries: I) -> Result<RunSteps, String>`, `RunSteps::apply(self, options: &mut BackupOptions)`. `BackupOptions.steps: BtrbkSteps` replaces `snapshot_only` and `send_only` (both removed).
- Consumes: nothing new. `apply` sets the existing `options.send_report`; Task 2 renames that field to `email_report` (and changes what it means), and its rename carries this line with it.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn btrbk_steps_from_the_two_ticks_and_nothing_ticked_is_refused() {
    assert_eq!(BtrbkSteps::from_ticks(true, true), Ok(BtrbkSteps::SnapshotAndSend));
    assert_eq!(BtrbkSteps::from_ticks(true, false), Ok(BtrbkSteps::SnapshotOnly));
    assert_eq!(BtrbkSteps::from_ticks(false, true), Ok(BtrbkSteps::SendOnly));
    let why = BtrbkSteps::from_ticks(false, false).unwrap_err();
    assert!(why.contains("neither Snapshot nor Send"), "{why}");
    assert!(BtrbkSteps::SnapshotOnly.snapshots() && !BtrbkSteps::SnapshotOnly.sends());
    assert!(!BtrbkSteps::SendOnly.snapshots() && BtrbkSteps::SendOnly.sends());
    assert!(BtrbkSteps::SnapshotAndSend.snapshots() && BtrbkSteps::SnapshotAndSend.sends());
}

fn entries(pairs: &[(&str, Option<bool>)]) -> Vec<(String, Option<bool>)> {
    pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
}

#[test]
fn run_steps_reads_every_key_and_each_one_reaches_its_own_field() {
    let all = [("snapshot", true), ("send", true), ("boot_archive", true), ("index", true), ("email", true)];
    // Flip one key at a time: a swapped key (index <-> email) fails here.
    for flipped in ["boot_archive", "index", "email"] {
        let pairs: Vec<_> = all.iter().map(|(k, v)| (*k, Some(if *k == flipped { !v } else { *v }))).collect();
        let s = RunSteps::from_entries(entries(&pairs)).unwrap();
        assert_eq!(s.boot_archive, flipped != "boot_archive", "{flipped}");
        assert_eq!(s.index, flipped != "index", "{flipped}");
        assert_eq!(s.email, flipped != "email", "{flipped}");
        assert_eq!(s.btrbk, BtrbkSteps::SnapshotAndSend);
    }
}

#[test]
fn run_steps_refuses_a_missing_unknown_or_non_boolean_key_and_never_defaults() {
    let full = [("snapshot", Some(true)), ("send", Some(true)), ("boot_archive", Some(true)), ("index", Some(true)), ("email", Some(true))];
    for missing in RUN_STEP_KEYS {
        let pairs: Vec<_> = full.iter().copied().filter(|(k, _)| *k != missing).collect();
        let why = RunSteps::from_entries(entries(&pairs)).unwrap_err();
        assert!(why.contains(missing) && why.contains("not given"), "{why}");
    }
    let mut extra = full.to_vec();
    extra.push(("preserve", Some(true)));
    assert!(RunSteps::from_entries(entries(&extra)).unwrap_err().contains("unknown step 'preserve'"));
    let mut not_bool = full.to_vec();
    not_bool[3] = ("index", None);
    assert!(RunSteps::from_entries(entries(&not_bool)).unwrap_err().contains("'index' is not a boolean"));
    assert!(RunSteps::from_entries(Vec::new()).is_err(), "an empty dictionary is refused");
    let mut neither = full.to_vec();
    neither[0] = ("snapshot", Some(false));
    neither[1] = ("send", Some(false));
    assert!(RunSteps::from_entries(entries(&neither)).unwrap_err().contains("neither Snapshot nor Send"));
}
```

Plus, in the existing scripted-runner tests for `run_pipeline` (search `btrbk("snapshot")`, `btrbk("resume")`, `btrbk("run")` near :3799-4386), add one test per (mode, steps) pair asserting the exact btrbk calls: Full+SnapshotAndSend → `[run]`; Full+SnapshotOnly → `[snapshot]`; Full+SendOnly → `[resume]`; Incremental+SnapshotAndSend → `[snapshot, resume]`; Incremental+SnapshotOnly → `[snapshot]`; Incremental+SendOnly → `[resume]`; and that a step not asked for leaves its count `Some(0)` (`Pipeline` doc, :1701). And a dry-run test: with `steps: SendOnly` the log has `would send to targets` and NOT `would create snapshots`; with `SnapshotOnly` the reverse.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-c4x-target cargo test --lib btrbk_steps run_steps`
Expected: compile errors — `BtrbkSteps`, `RunSteps`, `RUN_STEP_KEYS` not found.

- [ ] **Step 3: Implement**

```rust
/// Which btrbk steps a run performs. Snapshot and Send are the GUI's two
/// ticks; neither is not a choice — it is refused (bd c4x, as 7tx refuses
/// an empty selection), so it has no variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BtrbkSteps {
    /// Full: one `btrbk run`. Incremental: `btrbk snapshot`, then `btrbk resume`.
    #[default]
    SnapshotAndSend,
    /// `btrbk snapshot` only, in either mode.
    SnapshotOnly,
    /// `btrbk resume` only, in either mode: send what exists.
    SendOnly,
}

impl BtrbkSteps {
    pub fn from_ticks(snapshot: bool, send: bool) -> Result<Self, String> {
        match (snapshot, send) {
            (true, true) => Ok(Self::SnapshotAndSend),
            (true, false) => Ok(Self::SnapshotOnly),
            (false, true) => Ok(Self::SendOnly),
            (false, false) => Err("Nothing to do: neither Snapshot nor Send is selected — \
                                   refused, never read as both"
                .to_string()),
        }
    }
    pub fn snapshots(self) -> bool {
        !matches!(self, Self::SendOnly)
    }
    pub fn sends(self) -> bool {
        !matches!(self, Self::SnapshotOnly)
    }
}

/// The keys of the steps dictionary the GUI sends with `BackupRun`. Every
/// one must be present: a missing key is refused, never defaulted, so a GUI
/// and a helper from different builds fail loudly.
pub const RUN_STEP_KEYS: [&str; 5] = ["snapshot", "send", "boot_archive", "index", "email"];

/// What a GUI run was asked to do, read from the steps dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSteps {
    pub btrbk: BtrbkSteps,
    pub boot_archive: bool,
    pub index: bool,
    pub email: bool,
}

impl RunSteps {
    /// `entries` are the dictionary's (key, value) pairs; a value that is not
    /// a boolean arrives as `None`.
    pub fn from_entries<I: IntoIterator<Item = (String, Option<bool>)>>(
        entries: I,
    ) -> Result<Self, String> {
        let mut seen = std::collections::HashMap::new();
        for (key, value) in entries {
            if !RUN_STEP_KEYS.contains(&key.as_str()) {
                return Err(format!("unknown step '{key}' — refused"));
            }
            let Some(value) = value else {
                return Err(format!("step '{key}' is not a boolean — refused"));
            };
            seen.insert(key, value);
        }
        let get = |key: &str| {
            seen.get(key)
                .copied()
                .ok_or_else(|| format!("step '{key}' not given — refused, never defaulted"))
        };
        Ok(Self {
            btrbk: BtrbkSteps::from_ticks(get("snapshot")?, get("send")?)?,
            boot_archive: get("boot_archive")?,
            index: get("index")?,
            email: get("email")?,
        })
    }

    /// Put these steps into `options`.
    pub fn apply(self, options: &mut BackupOptions) {
        options.steps = self.btrbk;
        options.boot_archive = self.boot_archive;
        options.index_after = self.index;
        options.send_report = self.email; // `email_report` after Task 2
    }
}
```

In `BackupOptions` replace the two fields with:

```rust
    /// Which btrbk steps run (Snapshot, Send or both). Default: both.
    pub steps: BtrbkSteps,
```

`run_pipeline`'s `match mode { … }` (:1776-1800) becomes:

```rust
    match (mode, options.steps) {
        (_, BtrbkSteps::SnapshotOnly) => snapshots(&mut done),
        (_, BtrbkSteps::SendOnly) => send(&mut done),
        (BackupMode::Full, BtrbkSteps::SnapshotAndSend) => {
            match run_full_pipeline_with(config, sources, targets, progress, env) {
                Ok((created, sent, cleaned, bytes)) => {
                    done.created = Some(created);
                    done.sent = Some(sent);
                    done.cleaned = cleaned;
                    done.bytes = bytes;
                }
                Err(e) => {
                    // One btrbk run did all of it: what it created and sent is not known.
                    (done.created, done.sent) = (None, None);
                    done.failed(progress, format!("{}: {e}", BTRBK_STEP_FAILURES[2]));
                }
            }
        }
        (BackupMode::Incremental, BtrbkSteps::SnapshotAndSend) => {
            snapshots(&mut done);
            send(&mut done);
        }
    }
```

`total_steps` (:1940-1958): `steps.snapshots() as u64 + steps.sends() as u64 + boot_archive + index_after + 1` (the report is always written — Task 2). The dry-run block logs `would create snapshots for …` only when `options.steps.snapshots()`, `would send to targets …` only when `.sends()`, and one line each for index (`would index the targets afterwards`) and email (`would email the report` / `would save the report without emailing it`) — the boot line is Task 4's.

Fix every compile error the field removal raises (grep `snapshot_only\|send_only` across `indexer/`).

- [ ] **Step 4: Run the tests to see them pass**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-c4x-target cargo test && cargo test --features dbus`
Expected: all pass, 0 failed.

- [ ] **Step 5: Falsify** — swap the `"index"`/`"email"` lookups in `from_entries`: `run_steps_reads_every_key…` must go RED; make `(false,false)` return `SnapshotAndSend`: the refusal test must go RED. Restore (`git diff` empty for the swap) and record both RED lines for the commit message body.

- [ ] **Step 6: Commit**

```bash
git add indexer/src/backup.rs
git commit -S -m "$(cat <<'EOF'
Backup steps: Snapshot/Send as one enum, the GUI's steps dictionary read strictly

BtrbkSteps replaces snapshot_only/send_only (an impossible both-true state
gone); RunSteps reads the five keys and refuses a missing, unknown or
non-boolean one and a run with neither Snapshot nor Send (bd c4x).
EOF
)"
```

---

### Task 2: The report is always written; the Email tick mails it

**Files:**
- Modify: `indexer/src/backup.rs` (`BackupOptions.send_report` :147-148, `emails_report` :1528-1530, `deliver_report` :2193-2240), `indexer/src/main.rs` (`backup_run_options` :1280-1300 and its tests :2924-2930), `indexer/src/bin/btrdasd-helper.rs` (:341-343)
- Test: `indexer/src/backup.rs` `mod tests` (the `deliver_report` tests)

**Interfaces:**
- Produces: `BackupOptions.email_report: bool` — "mail the report too, when `[email]` is enabled". `send_report` is removed; `RunSteps::apply` (Task 1) now sets `email_report`. `deliver_report` always writes `[general].last_report`.

- [ ] **Step 1: Write the failing tests** — next to the existing `deliver_report` tests (search `fn deliver_report` callers in `mod tests`):

```rust
#[test]
fn the_report_is_written_whether_or_not_it_is_emailed() {
    for email_report in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = make_test_config();
        config.general.last_report = dir.path().join("last-report.txt").display().to_string();
        config.email.enabled = false; // nothing is mailed in a test
        let options = BackupOptions { email_report, ..Default::default() };
        let delivery = deliver_report(&config, &options, &sample_result(), &sample_report_data(), &TestProgress::new());
        assert_eq!(delivery.saved, Ok(()), "email_report={email_report}");
        assert!(Path::new(&config.general.last_report).is_file(), "email_report={email_report}: written");
        assert!(!delivery.emailed);
    }
}

#[test]
fn an_unticked_email_is_not_mailed_even_with_email_enabled() {
    let mut config = make_test_config();
    config.email.enabled = true;
    let options = BackupOptions { email_report: false, ..Default::default() };
    assert!(!emails_report(&options, &config));
    let options = BackupOptions { email_report: true, ..Default::default() };
    assert!(emails_report(&options, &config));
}
```

(`sample_result` / `sample_report_data`: use whichever helpers the existing `deliver_report` tests use; if none, build a `BackupResult` literal as at :2383 and `ReportData::default()`.)

- [ ] **Step 2: Run to see them fail**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-c4x-target cargo test --lib the_report_is_written an_unticked_email`
Expected: compile error — no field `email_report`.

- [ ] **Step 3: Implement** — rename the field with its doc comment `/// Mail the report as well, when [email] is enabled. The report itself is always written to [general].last_report (backup-run.sh writes $LAST_REPORT before any send).`; `emails_report` becomes `options.email_report && config.email.enabled`; delete the early return at :2200-2205 so the report is always written; `backup_run_options` (CLI) sets `email_report: true`; the helper sets it from `RunSteps` (Task 6) — for now `email_report: true` there too. Remove `send_report` from `total_steps` (the report step is counted always, Task 1).

- [ ] **Step 4: Run the full suites** — `cargo test` and `cargo test --features dbus`: all pass. `run_backup_job`'s "report lost" rule (:2483-2492) is unchanged and its tests must still pass.

- [ ] **Step 5: Falsify** — restore the early return guarded by `!options.email_report`: `the_report_is_written…` goes RED for `email_report=false`. Restore.

- [ ] **Step 6: Commit** — message: `Backup report: always written; the Email tick decides only whether it is mailed` with a body citing `backup.md` §Email Reports and bd c4x.

---

### Task 3: One boot plan and one snapshot-match rule, shared with the script

**Files:**
- Create: `tests/fixtures/boot-subvol-listing.txt`
- Modify: `indexer/src/backup.rs` (`find_latest_btrbk_snapshot` :1102-1116, `latest_matching_snapshot` :1120-1138, `subdirs_for_subvol` :1141-1152), `indexer/src/main.rs` (`BackupAction` :442-500 and its dispatch)
- Test: `indexer/src/backup.rs` `mod tests`; `indexer/tests/` CLI test if the crate has a CLI test file for `backup` (else in `main.rs` tests)

**Interfaces:**
- Produces: `pub struct BootPlanItem { pub subvol: String, pub snapshot_name: Option<String>, pub subdirs: Vec<String> }` (derive `Debug, Clone, PartialEq, Eq`); `pub fn boot_plan(config: &Config) -> Result<Vec<BootPlanItem>, String>` (Err = `btrbk.conf` unreadable, message names the path); `fn subvolume_listing(runner: &dyn CommandRunner, mount: &str) -> Result<String, String>` (Err = could not run, or nonzero exit, with stderr); `latest_matching_snapshot(listing, subdirs, snap_name) -> Option<&str>` keeps its signature but applies the match rule above. CLI: `btrdasd backup boot-plan [--config PATH]` prints one line per configured boot subvolume, `subvol<TAB>snapshot_name-or-"-"<TAB>comma-joined-subdirs-or-"-"`, exit 0; exit 2 with the reason on stderr when `btrbk.conf` cannot be read or a field contains a tab, newline or (subdirs) a comma.
- `find_latest_btrbk_snapshot` is deleted (its only caller is rewritten in Task 4).

- [ ] **Step 1: Create the shared fixture** `tests/fixtures/boot-subvol-listing.txt` (one header comment line is NOT allowed — the file is fed to both parsers verbatim):

```text
ID 300 gen 3 top level 5 path nvme/root-.20261004T0100
ID 301 gen 4 top level 5 path nvme/root-.20261005T0100
ID 302 gen 5 top level 5 path nvme/root-.20261005T0100_1
ID 303 gen 5 top level 5 path nvme/root-root.20261006T0100
ID 304 gen 5 top level 5 path nvme/root-.latest
ID 305 gen 6 top level 5 path nvme/root-.20261006T0100.new
ID 306 gen 6 top level 5 path nvmeX/root-.20261009T0100
ID 310 gen 7 top level 5 path nvme/home.20261005T0100
ID 311 gen 7 top level 5 path ssd/home.20261007T0100
ID 320 gen 8 top level 5 path nvme/a.b.20261003T0100
ID 321 gen 8 top level 5 path nvme/aXb.20261008T0100
ID 330 gen 9 top level 5 path @
ID 331 gen 9 top level 5 path @.archive.20261001T010203
```

Expected answers (both suites assert exactly these):

| subdirs | snapshot_name | newest |
|---|---|---|
| `nvme` | `root-` | `nvme/root-.20261005T0100_1` |
| `/nvme/` | `root-` | `nvme/root-.20261005T0100_1` |
| `nvme` | `home` | `nvme/home.20261005T0100` |
| `nvme,ssd` | `home` | `ssd/home.20261007T0100` |
| `nvme` | `a.b` | `nvme/a.b.20261003T0100` |
| `nvme` | `log` | none |

- [ ] **Step 2: Write the failing tests**

```rust
const SHARED_LISTING: &str = include_str!("../../tests/fixtures/boot-subvol-listing.txt");

#[test]
fn the_snapshot_match_rule_is_the_one_the_script_uses() {
    let cases: [(&[&str], &str, Option<&str>); 6] = [
        (&["nvme"], "root-", Some("nvme/root-.20261005T0100_1")),
        (&["/nvme/"], "root-", Some("nvme/root-.20261005T0100_1")),
        (&["nvme"], "home", Some("nvme/home.20261005T0100")),
        (&["nvme", "ssd"], "home", Some("ssd/home.20261007T0100")),
        (&["nvme"], "a.b", Some("nvme/a.b.20261003T0100")),
        (&["nvme"], "log", None),
    ];
    for (subdirs, name, want) in cases {
        let subdirs: Vec<String> = subdirs.iter().map(|s| s.to_string()).collect();
        assert_eq!(latest_matching_snapshot(SHARED_LISTING, &subdirs, name), want, "{subdirs:?} {name}");
    }
}

#[test]
fn a_listing_that_cannot_be_read_is_an_error_not_an_empty_listing() {
    let failed = Scripted::from_owned(vec![("btrfs subvolume list /m".into(), 1, String::new())])
        .with_stderr("btrfs subvolume list /m", "ERROR: can't access '/m'\n");
    let why = subvolume_listing(&failed, "/m").unwrap_err();
    assert!(why.contains("can't access"), "{why}");
    let ok = Scripted::from_owned(vec![("btrfs subvolume list /m".into(), 0, "ID 1 path x\n".into())]);
    assert_eq!(subvolume_listing(&ok, "/m").unwrap(), "ID 1 path x\n");
}

#[test]
fn the_boot_plan_names_come_from_btrbk_conf_and_an_unreadable_one_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (mut config, _conf) = archive_fixture(dir.path());
    config.boot.subvolumes = vec!["@".into(), "@home".into()];
    let plan = boot_plan(&config).unwrap();
    assert_eq!(plan[0], BootPlanItem { subvol: "@".into(), snapshot_name: Some("root-".into()), subdirs: vec!["nvme".into()] });
    assert_eq!(plan[1].subvol, "@home");
    assert_eq!(plan[1].snapshot_name, None, "absent from btrbk.conf: never guessed");
    config.general.btrbk_conf = "/nonexistent-c4x/btrbk.conf".into();
    assert!(boot_plan(&config).unwrap_err().contains("/nonexistent-c4x/btrbk.conf"));
}
```

(`Scripted::with_stderr` exists — see :3922. Adjust the key format to the one `Scripted` uses.) Add a CLI test: `btrdasd backup boot-plan --config <fixture config>` prints `@\troot-\tnvme` and, for a config whose `btrbk_conf` is missing, exits 2 with the path on stderr.

- [ ] **Step 3: Run to see them fail** — `cargo test --lib the_snapshot_match_rule a_listing_that the_boot_plan`: `root-root…`, `root-.latest`, `root-.…new` and `nvmeX/…` currently match the prefix rule, so the first test fails on the `nvme`/`root-` row (expects `…_1`, gets `nvme/root-.latest` or `root-.20261006T0100.new`); the other two fail to compile.

- [ ] **Step 4: Implement**

```rust
/// Whether `ts` is btrbk's `timestamp_format long` suffix: 8 digits, `T`,
/// 4 digits, optionally `_` and a collision number. ASCII only.
fn is_btrbk_timestamp(ts: &str) -> bool {
    let (stamp, collision) = match ts.split_once('_') {
        Some((s, n)) => (s, Some(n)),
        None => (ts, None),
    };
    let b = stamp.as_bytes();
    b.len() == 13
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..].iter().all(u8::is_ascii_digit)
        && collision.is_none_or(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}

fn latest_matching_snapshot<'a>(listing: &'a str, subdirs: &[String], snap_name: &str) -> Option<&'a str> {
    let prefixes: Vec<String> = subdirs
        .iter()
        .map(|d| format!("{}/{snap_name}.", d.trim_matches('/')))
        .collect();
    listing
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .filter(|path| {
            prefixes
                .iter()
                .any(|p| path.strip_prefix(p.as_str()).is_some_and(is_btrbk_timestamp))
        })
        .max()
}

fn subvolume_listing(runner: &dyn CommandRunner, mount: &str) -> Result<String, String> {
    let output = runner
        .output(Command::new("btrfs").args(["subvolume", "list", mount]))
        .map_err(|e| format!("btrfs could not be run: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "btrfs subvolume list {mount}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The boot subvolumes `[boot].subvolumes` names, each with the snapshot name
/// `btrbk.conf` gives it and the target subdirectories its snapshots land in.
/// What both `archive_boot` and `backup-run.sh` (through `backup boot-plan`)
/// act on, so the names are derived in one place (bd dtm).
pub fn boot_plan(config: &Config) -> Result<Vec<BootPlanItem>, String> {
    let conf = Path::new(&config.general.btrbk_conf);
    let names = crate::forget::live_subvol_snapshot_names(conf)
        .map_err(|e| format!("Cannot read {} ({e}) — no boot subvolume is touched rather than a snapshot name guessed", conf.display()))?;
    Ok(config
        .boot
        .subvolumes
        .iter()
        .map(|subvol| BootPlanItem {
            subvol: subvol.clone(),
            snapshot_name: names.get(subvol.as_str()).cloned(),
            subdirs: subdirs_for_subvol(config, subvol)
                .into_iter()
                .map(|d| d.trim_matches('/').to_string())
                .collect(),
        })
        .collect())
}
```

CLI: add `BootPlan { #[arg(long, default_value = DEFAULT_CONFIG)] config: PathBuf }` to `BackupAction` with doc `/// Print the boot subvolumes a full run refreshes, with their btrbk snapshot names and target subdirectories (tab-separated; read by backup-run.sh)`. Dispatch: load config (exit 2 on error, as other config readers), `boot_plan`, refuse (exit 2) a field containing `\t`/`\n` or a subdir containing `,`, else print `format!("{}\t{}\t{}", item.subvol, item.snapshot_name.as_deref().unwrap_or("-"), if item.subdirs.is_empty() { "-".into() } else { item.subdirs.join(",") })`.

- [ ] **Step 5: Run the suites** — all pass, including the existing `latest_matching_snapshot` tests (fix any that relied on the prefix-only rule only if their fixture is not a btrbk timestamp — say so in the commit body).

- [ ] **Step 6: Falsify** — drop the `is_btrbk_timestamp` filter: the shared-rule test goes RED on the `root-` rows. Make `subvolume_listing` return `Ok(String::new())` on a nonzero exit: its test goes RED. Restore both.

- [ ] **Step 7: Commit** — `Boot plan: snapshot names and subdirectories derived once, and a listing that cannot be read is an error` (body: fail-silent #6, bd dtm, the shared fixture).

---

### Task 4: The boot step classified — Rust path, report, history, CLI

**Files:**
- Modify: `indexer/src/backup.rs` (`archive_boot` :1180-1185, `archive_boot_with` :1188-1405, `BackupResult` :155-180, `run_backup_with` step (c) :2076-2086 and the dry-run block, `backup_summary` :2116-2160, `abort_job` :2383-2395, the record conversion near :6645), `indexer/src/report.rs` (:234-238, :260-267), `indexer/src/main.rs` (the `BootArchive` dispatch and every `BackupResult` literal), `.claude/rules/backup.md` §Boot Subvolume Archival
- Test: `indexer/src/backup.rs` and `indexer/src/report.rs` `mod tests`

**Interfaces:**
- Consumes: Task 3's `boot_plan`, `subvolume_listing`, `latest_matching_snapshot`, `BootPlanItem`.
- Produces:
  ```rust
  #[derive(Debug, Clone, Default, PartialEq, Eq)]
  pub struct BootOutcome { pub updated: usize, pub skipped: usize, pub warnings: Vec<String>, pub failures: Vec<String> }
  impl BootOutcome { pub fn status(&self) -> &'static str; pub fn detail(&self) -> String; fn warn(&mut self, p: &dyn ProgressCallback, msg: String); fn fail(&mut self, p: &dyn ProgressCallback, msg: String); }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum BootStep { NotSelected, DisabledInConfig, Ran(BootOutcome) }
  impl BootStep { pub fn archived(&self) -> bool; pub fn row(&self) -> String; pub fn has_warnings(&self) -> bool; }
  ```
  `BackupResult.boot: BootStep` replaces `boot_archived: bool`. `archive_boot(config, progress) -> BootStep` (replace = true). `archive_boot_with(config, selected, replace: bool, progress, env) -> BootStep` — never returns `NotSelected`.

- [ ] **Step 1: Write the failing tests** (rewrite the boot tests at :4926-5303 and :6097-6180 to the new return type; add):

```rust
#[test]
fn boot_outcome_status_and_detail_are_the_scripts_words() {
    let mut o = BootOutcome { updated: 1, skipped: 2, ..Default::default() };
    assert_eq!((o.status(), o.detail()), ("OK", "1 updated, 2 skipped".to_string()));
    o.warnings.push("w".into());
    assert_eq!((o.status(), o.detail()), ("WARN", "1 updated, 2 skipped, 1 warnings".to_string()));
    o.failures.push("f".into());
    assert_eq!((o.status(), o.detail()), ("FAIL", "1 updated, 1 failed".to_string()));
}

#[test]
fn an_incremental_run_creates_a_missing_boot_subvolume_and_never_replaces_one() {
    let dir = tempfile::tempdir().unwrap();
    let m = dir.path().display().to_string();
    let (config, _conf) = archive_fixture(dir.path());
    let list = (format!("btrfs subvolume list {m}"), 0, "ID 257 gen 9 top level 5 path nvme/root-.20261005T0100\n".to_string());
    // Absent: created from the newest snapshot.
    let runner = Scripted::from_owned(vec![list.clone()]).snapshotting();
    let step = archive_boot_with(&config, None, false, &TestProgress::new(), &env(&runner));
    assert_eq!(runner.calls(), [format!("btrfs subvolume list {m}"), format!("btrfs subvolume snapshot {m}/nvme/root-.20261005T0100 {m}/@")]);
    assert!(matches!(&step, BootStep::Ran(o) if o.updated == 1 && o.failures.is_empty()), "{step:?}");
    // Present: skipped, nothing archived, nothing deleted.
    std::fs::create_dir_all(dir.path().join("@")).unwrap();
    let runner = Scripted::from_owned(vec![list]).snapshotting();
    let step = archive_boot_with(&config, None, false, &TestProgress::new(), &env(&runner));
    assert_eq!(runner.calls(), [format!("btrfs subvolume list {m}")]);
    assert!(matches!(&step, BootStep::Ran(o) if o.skipped == 1 && o.updated == 0), "{step:?}");
}

#[test]
fn a_disabled_boot_step_runs_nothing_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (mut config, _conf) = archive_fixture(dir.path());
    config.boot.enabled = false;
    let runner = Scripted::from_owned(vec![]);
    assert_eq!(archive_boot_with(&config, None, true, &TestProgress::new(), &env(&runner)), BootStep::DisabledInConfig);
    assert!(runner.calls().is_empty());
    assert_eq!(BootStep::DisabledInConfig.row(), "OK  (disabled in config)");
    assert_eq!(BootStep::NotSelected.row(), "N/A  (not selected)");
}

#[test]
fn every_failure_of_the_boot_step_fails_the_run_and_every_absence_only_warns() {
    // FAIL: unreadable btrbk.conf, unreadable listing, each of the five
    // replacement failures (reuse the cases of
    // boot_archive_never_loses_the_live_subvolume_whichever_step_fails plus
    // the stale-staging and rename cases). WARN: no snapshot_name, no
    // target_subdirs, no snapshot of the series. Assert for each case:
    //   status(), the counts, and that run_backup_with's result.success is
    //   false exactly for the FAIL cases (drive run_backup_with with
    //   boot_archive: true, mode Full, as the existing run_backup_with tests do).
}
```

Write the body of the last test as one table-driven loop over those ten cases — it is the test that pins the classification table, so do not shorten it. In `report.rs` tests: a result with `BootStep::Ran(o)` where `o` has one warning and `success: true` renders `Status: COMPLETED WITH WARNINGS` and `Boot subvolumes       WARN  (0 updated, 0 skipped, 1 warnings)`; with a failure, `FAILURES DETECTED` and `FAIL  (0 updated, 1 failed)`. A dry-run test: `boot_archive: true`, Full → log has `would archive and replace boot subvolumes`; Incremental → `would create any missing boot subvolume (never replace one)`; `[boot] enabled = false` → `boot subvolumes: disabled in config`.

- [ ] **Step 2: Run to see them fail** — compile errors (`BootOutcome`, `BootStep`, the `replace` parameter).

- [ ] **Step 3: Implement `BootOutcome` / `BootStep`**

```rust
impl BootOutcome {
    pub fn status(&self) -> &'static str {
        if !self.failures.is_empty() { "FAIL" } else if !self.warnings.is_empty() { "WARN" } else { "OK" }
    }
    /// `backup-run.sh` records `boot_subvols` with these words.
    pub fn detail(&self) -> String {
        match self.status() {
            "FAIL" => format!("{} updated, {} failed", self.updated, self.failures.len()),
            "WARN" => format!("{} updated, {} skipped, {} warnings", self.updated, self.skipped, self.warnings.len()),
            _ => format!("{} updated, {} skipped", self.updated, self.skipped),
        }
    }
    fn warn(&mut self, progress: &dyn ProgressCallback, msg: String) {
        progress.on_log(LogLevel::Warning, &msg);
        self.warnings.push(msg);
    }
    fn fail(&mut self, progress: &dyn ProgressCallback, msg: String) {
        progress.on_log(LogLevel::Error, &msg);
        self.failures.push(msg);
    }
}

impl BootStep {
    /// Whether any boot subvolume was created or replaced (`backup_runs.boot_archived`).
    pub fn archived(&self) -> bool {
        matches!(self, Self::Ran(o) if o.updated > 0)
    }
    pub fn has_warnings(&self) -> bool {
        matches!(self, Self::Ran(o) if !o.warnings.is_empty())
    }
    /// The report's `Boot subvolumes` cell.
    pub fn row(&self) -> String {
        match self {
            Self::NotSelected => "N/A  (not selected)".into(),
            Self::DisabledInConfig => "OK  (disabled in config)".into(),
            Self::Ran(o) => format!("{}  ({})", o.status(), o.detail()),
        }
    }
}
```

- [ ] **Step 4: Rewrite `archive_boot_with`** — target-outer, subvolume-inner (the script's order, so the counts mean the same thing), returning `BootStep`:

```rust
fn archive_boot_with(config: &Config, selected: Option<&[String]>, replace: bool, progress: &dyn ProgressCallback, env: &StepEnv) -> BootStep {
    if !config.boot.enabled {
        progress.on_log(LogLevel::Info, "Boot subvolumes: disabled in config ([boot] enabled = false)");
        return BootStep::DisabledInConfig;
    }
    let mut out = BootOutcome::default();
    if config.targets.is_empty() {
        out.warn(progress, "No backup targets configured — no boot subvolume updated".into());
        return BootStep::Ran(out);
    }
    let is_selected = |t: &Target| selected.is_none_or(|labels| labels.contains(&t.label));
    let writes: Vec<String> = config.targets.iter()
        .filter(|t| is_selected(t) && t.role != TargetRole::Mirror)
        .filter(|t| Path::new(&t.mount).exists())
        .map(|t| t.label.clone())
        .collect();
    if let Err(e) = (env.verify)(&config.targets, &writes, progress) {
        out.fail(progress, format!("Boot subvolumes not updated: {e}"));
        return BootStep::Ran(out);
    }
    let plan = match boot_plan(config) {
        Ok(plan) => plan,
        Err(e) => { out.fail(progress, e); return BootStep::Ran(out); }
    };
    let ts = format_timestamp();
    let targets: Vec<&Target> = config.targets.iter().filter(|t| is_selected(t)).collect();
    progress.on_stage("Boot subvolumes", targets.len() as u64);
    for (i, target) in targets.iter().enumerate() {
        progress.on_progress(i as u64, targets.len() as u64, &format!("Boot subvolumes on {}", target.label));
        if target.role == TargetRole::Mirror {
            // Mirror targets carry an independent OS in their own @/@home — never
            // replaced with a host snapshot (bd am1; same words as the script).
            progress.on_log(LogLevel::Info, &format!("[{}] Skipping mirror target (independent OS)", target.mount));
            out.skipped += 1;
            continue;
        }
        if !writes.contains(&target.label) {
            progress.on_log(LogLevel::Info, &format!("[{}] Not mounted — boot subvolumes not updated on '{}'", target.mount, target.label));
            continue;
        }
        let listing = match subvolume_listing(env.runner, &target.mount) {
            Ok(listing) => listing,
            Err(e) => {
                out.fail(progress, format!("[{}] Could not list subvolumes ({e}) — refusing to read an unreadable target as 'no snapshots'", target.label));
                continue;
            }
        };
        for item in &plan {
            update_boot_subvol(env.runner, target, item, &listing, replace, &ts, &mut out, progress);
        }
    }
    progress.on_progress(targets.len() as u64, targets.len() as u64, "Boot subvolumes done");
    BootStep::Ran(out)
}
```

`update_boot_subvol` (new, private; `#[allow(clippy::too_many_arguments)]` if clippy asks): WARN for no `snapshot_name` / no subdirs / no matching snapshot (messages: `[<label>] <subvol> has no snapshot_name in <btrbk.conf> — leaving it untouched`, `[<label>] No source declares target_subdirs for <subvol> — leaving it untouched`, `[<label>] No btrbk snapshot named '<name>' — leaving <subvol> untouched`); when `<mount>/<subvol>` exists and `!replace`: Info `[<label>] <subvol> exists, skipping (a full run replaces it)`, `skipped += 1`; when it exists and `replace`: the existing six-step order (archive `-r` → clear stale `.new` → build `.new` → delete live → rename), each failure `out.fail(...)` with the existing message text (`Failed to archive …`, `Stale … could not be removed …`, `Failed to create …`, `Failed to delete … — discarding …`, `Renamed nothing: …`) and `return`; success `updated += 1` with Info `Recreated <live> from <latest> (archived old <subvol> -> <archive name>)`; when absent (either mode): `btrfs subvolume snapshot <latest> <live>`, success `updated += 1` with Info `Created <live> from <latest>`, failure `out.fail` `Failed to create <live> from <latest>`. A `btrfs` that cannot be spawned (`btrfs_ok` → `Err`) is `out.fail(format!("btrfs could not be run: {e}"))` and `return`.

`archive_boot` (CLI `backup boot-archive`) passes `replace: true`. `run_backup_with` step (c):

```rust
    // Step (c): boot subvolumes — create a missing one on every run, archive
    // and replace on a full run, as backup-run.sh's update_boot_subvolumes does
    // (bd woq, dtm). A failure fails the run; an absence only warns.
    let boot = if options.boot_archive {
        let step = archive_boot_with(config, Some(&effective_targets), mode == BackupMode::Full, progress, env);
        if let BootStep::Ran(o) = &step {
            errors.extend(o.failures.iter().map(|f| format!("Boot subvolumes: {f}")));
        }
        step
    } else {
        BootStep::NotSelected
    };
```

Replace `boot_archived` everywhere (`grep -rn boot_archived indexer/`): the field becomes `boot`, literals use `BootStep::NotSelected` (or the case under test), readers use `.archived()`. `backup_summary` prints `boot subvolumes: {row}` instead of `boot archived: {bool}` (update its tests). `report.rs`: the boot row is `format!("  Boot subvolumes       {}\n", result.boot.row())`; `overall` becomes `if !result.success { "FAILURES DETECTED" } else if result.boot.has_warnings() { "COMPLETED WITH WARNINGS" } else { "ALL OPERATIONS SUCCESSFUL" }`; when the boot step ran with warnings or failures, append a `BOOT SUBVOLUMES` section listing each message (after `BACKUP OPERATIONS`). CLI `backup boot-archive`: print `Boot subvolumes: <row>` and each message; exit 3 when the status is FAIL, 0 otherwise (exit 1 is unchanged for could-not-start). Dry run: one line per the Step 1 dry-run test.

- [ ] **Step 5: Write the classification table into `.claude/rules/backup.md`** §Boot Subvolume Archival, replacing the bullet "Two code paths, which must stay symmetric…" with: the bullet kept, plus "**One classification, both paths** (bd woq, dtm, 2026-10-06):" and the table from this plan's "The boot-step classification" section, and the match rule. Keep the file's style (binding rules only; reasoning to `.claude/docs/rules-reference/backup.md` under the same heading — add a short paragraph there citing the operator decisions of 2026-10-06).

- [ ] **Step 6: Run the suites** — `cargo test`, `cargo test --features dbus`, clippy, fmt: all green.

- [ ] **Step 7: Falsify** — (a) in `run_backup_with`, drop the `errors.extend(...)`: the classification test goes RED for every FAIL case; (b) pass `replace: true` unconditionally: the incremental test goes RED (an archive call appears); (c) make `BootStep::has_warnings` return `false`: the report WARN test goes RED. Restore each.

- [ ] **Step 8: Commit** — `Boot step: one FAIL/WARN/skip classification; incremental creates, full replaces; failures fail the run` (body: bd woq, dtm, the table's home in backup.md).

---

### Task 5: `backup-run.sh` honours `[boot]` and the shared plan (dtm)

**Files:**
- Modify: `scripts/backup-run.sh` (`update_boot_subvolumes` :1929-2128; its call site near the `--full` gate — find with `grep -n 'update_boot_subvolumes' scripts/backup-run.sh`; header version and changelog lines :3-30)
- Modify: `tests/test_early_exit_readers.sh` (:112-270, the boot cases), `tests/test_backup_exit_semantics.sh` (:2050 and its stub `btrdasd`), and the stub `btrdasd` the shell suites use (find with `grep -rln 'dump-env' tests/`)
- Test: a new section "boot plan and the shared listing" in `tests/test_early_exit_readers.sh` (it already extracts `update_boot_subvolumes`)

**Interfaces:**
- Consumes: Task 3's `btrdasd backup boot-plan` (TSV, exit 2 on error); `DAS_BOOT_ENABLED` from `config dump-env` (`indexer/src/setup/env_export.rs:43`).
- Produces: `latest_boot_snapshot <listing> <snapshot_name> <subdirs-csv>` — sets the global `LATEST_BOOT_SNAPSHOT` (empty = none), no subshell, no pipe, no fd; `update_boot_subvolumes [force]` records `boot_subvols` with Task 4's status words.

- [ ] **Step 1: Write the failing tests** — in the new section, source the extracted functions and assert, for each row of Task 3's answer table, `latest_boot_snapshot "$(cat tests/fixtures/boot-subvol-listing.txt)" <name> <subdirs>` leaves `LATEST_BOOT_SNAPSHOT` equal to the table's answer (`""` for none). Then, with the stub `btrdasd` answering `backup boot-plan` and the stub `btrfs` answering `subvolume list|show|snapshot|delete` from fixtures (extend the existing `run_boot_subvols` helper :132-226):
  1. `DAS_BOOT_ENABLED=false` → `OK|disabled in config`, zero `btrfs` calls.
  2. boot-plan exits 2 → `FAIL|0 updated, 1 failed`, zero `btrfs subvolume snapshot` calls.
  3. plan `@\troot-\tnvme` + `@home\t-\tnvme`, shared listing, `@` absent, not full → `WARN|1 updated, 0 skipped, 1 warnings`, and a `btrfs subvolume snapshot <mnt>/nvme/root-.20261005T0100_1 <mnt>/@` call.
  4. the same with `@` present, not full → `WARN|0 updated, 1 skipped, 1 warnings`, no snapshot call.
  5. full, `@` present, archive fails → `FAIL|0 updated, 1 failed`, no delete call.
  6. the existing `drift-big` listing (> 64 KiB) with a matching series appended at its end, full → the appended snapshot is the one used (Review Focus 3), under `set -o pipefail`.
  7. the existing "could not list" and "could not tell whether mounted" cases keep their FAIL results.
  Remove the "drift" expectations (`Target HAS btrbk-shaped snapshots…`): names now come from `btrbk.conf`, so a listing with other series is a WARN, not a pattern defect — rewrite those cases to expect `WARN`, and say so in the test comment.

- [ ] **Step 2: Run to see them fail** — `ctest --test-dir /tmp/das-c4x-build -R early-exit --output-on-failure`: the new cases fail (`latest_boot_snapshot: command not found`; disabled case records `OK|0 updated…` after calling btrfs).

- [ ] **Step 3: Implement**

```bash
# latest_boot_snapshot <listing> <snapshot_name> <subdirs, comma-separated>:
# the newest "<subdir>/<snapshot_name>.<TS>" in a `btrfs subvolume list`
# listing, in LATEST_BOOT_SNAPSHOT ("" = none). The rule is the Rust
# library's (backup::latest_matching_snapshot), pinned for both by
# tests/fixtures/boot-subvol-listing.txt. Walked by parameter expansion —
# no pipe (SIGPIPE under pipefail, bd wkvz), no here-string (a temp file:
# a full /tmp read as "none"), no subshell. Literal prefix match, so a
# snapshot name holding '.', '*' or '-' matches only itself.
latest_boot_snapshot() {
    local listing=$1 name=$2 subdirs=$3 line path dir prefix ts rest
    local LC_ALL=C
    local -a dirs=()
    rest=$subdirs
    while [[ -n $rest ]]; do
        dir=${rest%%,*}
        if [[ $rest == *,* ]]; then rest=${rest#*,}; else rest=""; fi
        dir=${dir#/}
        dir=${dir%/}
        [[ -n $dir ]] && dirs+=("$dir")
    done
    LATEST_BOOT_SNAPSHOT=""
    rest=$listing
    while [[ -n $rest ]]; do
        line=${rest%%$'\n'*}
        if [[ $rest == *$'\n'* ]]; then rest=${rest#*$'\n'}; else rest=""; fi
        path=${line##* }
        for dir in "${dirs[@]}"; do
            prefix="$dir/$name."
            [[ $path == "$prefix"* ]] || continue
            ts=${path#"$prefix"}
            [[ $ts =~ ^[[:digit:]]{8}T[[:digit:]]{4}(_[[:digit:]]+)?$ ]] || continue
            if [[ -z $LATEST_BOOT_SNAPSHOT || $path > $LATEST_BOOT_SNAPSHOT ]]; then
                LATEST_BOOT_SNAPSHOT=$path
            fi
        done
    done
}
```

`update_boot_subvolumes`: at the top, `if [[ "${DAS_BOOT_ENABLED:-}" != true ]]; then log_info "Boot subvolumes: disabled in config ([boot] enabled = false)"; record_op "boot_subvols" "OK" "disabled in config"; return; fi`. Then read the plan once — declare first, assign separately (never `local x=$(cmd)`):

```bash
    local plan plan_err
    if ! plan_err=$(mktemp); then
        log_error "  Could not make a temp file for the boot plan's errors — boot subvolumes NOT updated"
        record_op "boot_subvols" "FAIL" "0 updated, 1 failed"
        return
    fi
    if ! plan=$("$BTRDASD_BIN" backup boot-plan --config "$DAS_CONFIG" 2>"$plan_err"); then
        log_error "  Could not read the boot plan: $(tr '\n' ' ' <"$plan_err") — boot subvolumes NOT updated"
        rm -f "$plan_err"
        record_op "boot_subvols" "FAIL" "0 updated, 1 failed"
        return
    fi
    rm -f "$plan_err"
```

Keep the target loop's probe, mirror and listing blocks as they are (:1942-2021). Replace :2023-2120 with a walk over the plan's lines (same parameter-expansion pattern; fields split with `${line%%$'\t'*}` etc.), and per item: `-` snapshot name → `log_warn` + `(( warned += 1 ))`; `-` subdirs → same; `latest_boot_snapshot "$subvol_listing" "$name" "$subdirs"`, empty → `log_warn "  [$label] No btrbk snapshot named '$name' — leaving $subvol untouched"` + warned; then the existing @/@home logic generalised to `$subvol` (`$mnt/$subvol`, `$mnt/$subvol.archive.$ts`, `$mnt/$subvol.new`), with the existing message texts (`@` replaced by `$subvol`) and `(use --full to recreate)` kept. Final record:

```bash
    if (( failed > 0 )); then
        record_op "boot_subvols" "FAIL" "$updated updated, $failed failed"
    elif (( warned > 0 )); then
        record_op "boot_subvols" "WARN" "$updated updated, $skipped skipped, $warned warnings"
    else
        record_op "boot_subvols" "OK" "$updated updated, $skipped skipped"
    fi
```

Verify (read the code, then a stub run) that `record_op … WARN` makes the report say `COMPLETED WITH WARNINGS` and the script exit 0 — it does for `recovery_os`; if `boot_subvols` WARN is not mapped the same way, map it. Bump the header to `# Version: 4.12.0` with one changelog line in the header's style: `#   - The boot step honours [boot] and reads its names from btrbk.conf (v4.12.0): …`.

- [ ] **Step 4: Run** — `ctest --test-dir /tmp/das-c4x-build --output-on-failure` with `-DREQUIRE_ALL_SHELL_CASES=ON`: 100% passed; `shellcheck scripts/backup-run.sh tests/*.sh` clean; the 1bsx lint (`shell-early-exit-readers`) green.

- [ ] **Step 5: Falsify** — (a) remove the `DAS_BOOT_ENABLED` gate: case 1 RED; (b) drop the `ts =~` check: the shared-fixture rows RED (`root-.latest` / `.new` win); (c) replace the parameter-expansion walk with `printf '%s\n' "$listing" | while read …` under pipefail: case 6 RED. Restore each.

- [ ] **Step 6: Commit** — `backup-run.sh 4.12.0: the boot step honours [boot] and the names btrbk.conf gives (bd dtm)`.

---

### Task 6: The helper's `BackupRun` takes the steps; the half-selection calls go

**Files:**
- Modify: `indexer/src/bin/btrdasd-helper.rs` (`backup_run` :313-384; delete `backup_snapshot` :386-466, `backup_send` :468-560, `backup_boot_archive` :562-636 and any `use` they alone needed)
- Test: `indexer/src/bin/btrdasd-helper.rs` `mod tests` (:1805-)

**Interfaces:**
- Consumes: Task 1's `RunSteps::from_entries`, `RunSteps::apply`.
- Produces: D-Bus `BackupRun(s mode, as sources, as targets, b dry_run, a{sv} steps) -> s job_id`; `fn step_entries(map: HashMap<String, zbus::zvariant::OwnedValue>) -> Vec<(String, Option<bool>)>`; `fn parse_mode(mode: &str) -> Result<BackupMode, String>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_step_value_that_is_not_a_boolean_reaches_the_library_as_none() {
    use zbus::zvariant::{OwnedValue, Value};
    let mut map = std::collections::HashMap::new();
    map.insert("index".to_string(), OwnedValue::try_from(Value::from(true)).unwrap());
    map.insert("email".to_string(), OwnedValue::try_from(Value::from("yes")).unwrap());
    let mut got = step_entries(map);
    got.sort();
    assert_eq!(got, [("email".to_string(), None), ("index".to_string(), Some(true))]);
}

#[test]
fn an_unknown_or_empty_mode_is_refused_never_read_as_incremental() {
    assert_eq!(parse_mode("full"), Ok(BackupMode::Full));
    assert_eq!(parse_mode("Incremental"), Ok(BackupMode::Incremental));
    for bad in ["", "fulll", "snapshot"] {
        assert!(parse_mode(bad).unwrap_err().contains("mode"), "{bad:?}");
    }
}
```

And a source-level check that the three removed methods are gone from the interface (so a stale caller fails with UnknownMethod): `assert!(!include_str!("btrdasd-helper.rs").contains("async fn backup_send("))` — or, better if the crate has an introspection test, assert the introspected method list.

- [ ] **Step 2: Run to see them fail** — `cargo test --features dbus --bin btrdasd-helper`: compile errors.

- [ ] **Step 3: Implement**

```rust
/// The steps dictionary as (key, value) pairs; a value that is not a
/// boolean becomes `None`, which `RunSteps::from_entries` refuses.
fn step_entries(map: HashMap<String, zbus::zvariant::OwnedValue>) -> Vec<(String, Option<bool>)> {
    map.into_iter()
        .map(|(key, value)| (key, bool::try_from(&value).ok()))
        .collect()
}

/// `mode` as the GUI sends it. Anything else is refused: an unknown mode
/// read as incremental would be a setting accepted and ignored.
fn parse_mode(mode: &str) -> Result<BackupMode, String> {
    match mode.to_lowercase().as_str() {
        "full" => Ok(BackupMode::Full),
        "incremental" => Ok(BackupMode::Incremental),
        other => Err(format!("unknown backup mode {other:?} — refused (full or incremental)")),
    }
}
```

(Use whichever `TryFrom` zvariant provides for `bool` from an `OwnedValue` or `&OwnedValue`; the test above is the contract.) `backup_run` gains `steps: HashMap<String, zbus::zvariant::OwnedValue>` after `dry_run`; after the polkit check: `let mode = parse_mode(mode).map_err(fdo::Error::InvalidArgs)?; let steps = RunSteps::from_entries(step_entries(steps)).map_err(fdo::Error::InvalidArgs)?;`, then build `BackupOptions { mode: Some(mode), sources: Some(sources), targets: Some(targets), dry_run, ..Default::default() }` and `steps.apply(&mut options)`. Delete the three methods. Update the method's doc comment: `/// Run a backup job with the operations the GUI ticked (bd c4x).`

- [ ] **Step 4: Run** — `cargo test --features dbus`, clippy (dbus feature): green. `grep -rn 'BackupSnapshot\|BackupSend\|BackupBootArchive\|backup_snapshot\|backup_send\|backup_boot_archive' --exclude-dir=.git --exclude-dir=.snapshots .` — only historical `docs/plans/2026-0*` and CHANGELOG history may remain.

- [ ] **Step 5: Falsify** — make `parse_mode`'s fallback `Ok(BackupMode::Incremental)`: the mode test goes RED. Restore.

- [ ] **Step 6: Commit** — `Helper: BackupRun carries the ticked steps; BackupSnapshot/Send/BootArchive removed (bd c4x, rbp M9)`.

---

### Task 7: The GUI sends what is ticked

**Files:**
- Create: `gui/src/backupsteps.h`, `gui/src/backupsteps.cpp`
- Modify: `gui/src/backuppanel.cpp`, `gui/src/backuppanel.h`, `gui/src/dbusclient.cpp` (:85-114), `gui/src/dbusclient.h` (:22-27), `gui/CMakeLists.txt` (`target_sources` :35 and `gui-smoketest` :99-108)
- Test: `gui/tests/smoketest.cpp`

**Interfaces:**
- Consumes: Task 6's D-Bus signature and `RUN_STEP_KEYS` spelling.
- Produces: `struct BackupSteps { bool snapshot = true; bool send = true; bool bootArchive = true; bool index = true; bool email = true; [[nodiscard]] bool runsBtrbk() const; [[nodiscard]] QVariantMap toDBus() const; };`; `void DBusClient::backupRun(const QString &mode, const QStringList &sources, const QStringList &targets, bool dryRun, const BackupSteps &steps);`. `backupSnapshot`, `backupSend`, `backupBootArchive` removed.

- [ ] **Step 1: Write the failing smoke tests**

```cpp
    void backupStepsMapToTheHelpersKeys()
    {
        const BackupSteps all;
        const QVariantMap map = all.toDBus();
        QCOMPARE(map.keys(), (QStringList{QStringLiteral("boot_archive"), QStringLiteral("email"),
                                          QStringLiteral("index"), QStringLiteral("send"),
                                          QStringLiteral("snapshot")}));
        // One box at a time: each reaches its own key and no other.
        const QList<std::pair<bool BackupSteps::*, QString>> boxes{
            {&BackupSteps::snapshot, QStringLiteral("snapshot")},
            {&BackupSteps::send, QStringLiteral("send")},
            {&BackupSteps::bootArchive, QStringLiteral("boot_archive")},
            {&BackupSteps::index, QStringLiteral("index")},
            {&BackupSteps::email, QStringLiteral("email")},
        };
        for (const auto &[member, key] : boxes) {
            BackupSteps s;
            s.*member = false;
            const QVariantMap m = s.toDBus();
            for (auto it = m.cbegin(); it != m.cend(); ++it) {
                QCOMPARE(it.value().metaType().id(), QMetaType::Bool);
                QCOMPARE(it.value().toBool(), it.key() != key);
            }
        }
    }

    void nothingToRunWithoutSnapshotOrSend()
    {
        BackupSteps s;
        QVERIFY(s.runsBtrbk());
        s.snapshot = false;
        QVERIFY(s.runsBtrbk());
        s.send = false;
        QVERIFY(!s.runsBtrbk());
        s.snapshot = true;
        QVERIFY(s.runsBtrbk());
    }
```

Add `#include "../src/backupsteps.h"` and `src/backupsteps.cpp` to the `gui-smoketest` sources.

- [ ] **Step 2: Run to see them fail** — `cmake --build /tmp/das-c4x-build && ctest --test-dir /tmp/das-c4x-build -R gui-smoketest --output-on-failure`: build error, `backupsteps.h` not found.

- [ ] **Step 3: Implement `BackupSteps`**

```cpp
// backupsteps.h
#pragma once

#include <QVariantMap>

// The operations a GUI backup run performs, as ticked. Sent with BackupRun as
// its steps dictionary (a{sv}); the helper refuses a missing or unknown key,
// a value that is not a boolean, and a run with neither Snapshot nor Send
// (bd DAS-Backup-Manager-c4x). The keys are backup::RUN_STEP_KEYS.
struct BackupSteps {
    bool snapshot = true;
    bool send = true;
    bool bootArchive = true;
    bool index = true;
    bool email = true;

    [[nodiscard]] bool runsBtrbk() const { return snapshot || send; }
    [[nodiscard]] QVariantMap toDBus() const;
};
```

```cpp
// backupsteps.cpp
#include "backupsteps.h"

QVariantMap BackupSteps::toDBus() const
{
    return {
        {QStringLiteral("snapshot"), snapshot},
        {QStringLiteral("send"), send},
        {QStringLiteral("boot_archive"), bootArchive},
        {QStringLiteral("index"), index},
        {QStringLiteral("email"), email},
    };
}
```

- [ ] **Step 4: Wire the client and the panel**
  - `DBusClient::backupRun` gains `const BackupSteps &steps` and passes `steps.toDBus()` as the fifth argument (a `QVariantMap` marshals as `a{sv}`). Delete `backupSnapshot`, `backupSend`, `backupBootArchive` (header and source).
  - `BackupPanel::loadConfig`: add `Section::Boot` for the `[boot]` header; `enabled = false` there sets `m_bootEnabledInConfig = false` (member, default `true`, reset to `true` at the start of every load).
  - New private slot `updateBootArchive()`, connected to `m_fullRadio`'s and `m_incrementalRadio`'s `toggled` and called at the end of `loadConfig`: when `!m_bootEnabledInConfig`: `setChecked(false)`, `setEnabled(false)`, tooltip `i18n("Disabled in config.toml ([boot] enabled = false)")`; else `setEnabled(true)` and the tooltip for the mode — Full: `i18n("Archive each [boot] subvolume (@, @home) on primary targets read-only, then replace it from the newest snapshot. Mirror targets are never touched.")`; Incremental: `i18n("Create a [boot] subvolume (@, @home) that is missing on a primary target from the newest snapshot. An existing one is never replaced. Mirror targets are never touched.")`.
  - Tooltips: Email `i18n("Email the report after the run (when [email] is enabled in config). The report is always saved.")`.
  - `updateRunEnabled`: `selected = anyChecked(sources) && anyChecked(targets) && (m_snapshotCheck->isChecked() || m_sendCheck->isChecked())`; the status tip names what is missing (`i18n("Tick Snapshot or Send")` when that is the gap). Connect `m_snapshotCheck` and `m_sendCheck` `toggled` to it.
  - `runBackup`: build `BackupSteps` from the five boxes; `if (sources.isEmpty() || targets.isEmpty() || !steps.runsBtrbk()) return;` (comment: the buttons are disabled in that state; this keeps a stray call from sending it); `m_client->backupRun(mode, sources, targets, dryRun, steps);`.
  - Give the five boxes and two buttons `setObjectName` (`snapshotCheck`, `sendCheck`, `bootArchiveCheck`, `indexCheck`, `emailCheck`, `dryRunButton`, `runButton`) for the operator check and future tests.

- [ ] **Step 5: Run** — full build, `ctest` 100%, `clang-tidy`/`cppcheck` on the changed C++ files (the `memory-safety-c-cpp.md` rule): no new findings.

- [ ] **Step 6: Falsify** — swap `index`/`email` in `toDBus`: `backupStepsMapToTheHelpersKeys` RED; make `runsBtrbk` return `true`: the second test RED. Restore.

- [ ] **Step 7: Commit** — `GUI: every operation box reaches the helper; Run needs Snapshot or Send; Boot Archive says what it does in each mode (bd c4x, woq)`.

---

### Task 8: `Next scheduled:` is never blank (bd hyvh)

**Files:**
- Modify: `scripts/backup-run.sh` (:2855, the report line; add `next_scheduled` near the report helpers)
- Test: the report section of `tests/test_backup_exit_semantics.sh` (its stub `systemctl`)

- [ ] **Step 1: Measure the empty value** (turns the issue's hypothesis into a fact or refutes it; record the output in the commit body):

```bash
sudo systemd-run --unit=das-hyvh-probe --on-calendar='*:*:00' --timer-property=AccuracySec=1s /bin/sleep 45
# wait until the service is active (poll systemctl is-active das-hyvh-probe.service), then:
systemctl show -P NextElapseUSecRealtime das-hyvh-probe.timer; echo "rc=$?"
# after the service ends:
systemctl show -P NextElapseUSecRealtime das-hyvh-probe.timer
sudo systemctl stop das-hyvh-probe.timer das-hyvh-probe.service
```

If the value is NOT empty while active, the cause is elsewhere: stop, write what was seen into bd hyvh (`--append-notes`), and report back before changing the script.

- [ ] **Step 2: Write the failing tests** — stub `systemctl show … NextElapseUSecRealtime` printing `""` → report line `  Next scheduled: unknown`; printing `Wed 2026-10-07 03:05:47 CDT` → `  Next scheduled: Wed 2026-10-07 03:05:47`; stub exiting 1 → `unknown`.

- [ ] **Step 3: Implement**

```bash
# next_scheduled: the timer's next run for the report, or "unknown". Empty
# while the timer's own run is going (measured: see bd hyvh) — the old
# `… | sed … || echo unknown` could never print unknown: sed exits 0 on
# empty input, so the blank went into the report.
next_scheduled() {
    local next
    if ! next=$(systemctl show das-backup.timer --property=NextElapseUSecRealtime --value); then
        next=""
    fi
    next=${next% [[:upper:]]*}
    if [[ -z $next || $next == "n/a" ]]; then
        next="unknown"
    fi
    printf '%s' "$next"
}
```

and the report line becomes `  Next scheduled: $(next_scheduled)`.

- [ ] **Step 4: Run** — the suite green; shellcheck clean.
- [ ] **Step 5: Falsify** — restore the old one-liner: the empty-value test RED. Restore.
- [ ] **Step 6: Commit** — `backup-run.sh: a timer with no next run time prints unknown, never a blank (bd hyvh)`.

---

### Task 9: No false "already mounted" warning under a handed-down lock (bd 8veh)

**Files:**
- Modify: `indexer/src/maintenance.rs` (`impl MaintenanceHeld`, :127-), `indexer/src/mount.rs` (:370-390 and the twin at :1828)
- Test: `indexer/src/mount.rs` `mod tests`

**Interfaces:**
- Produces: `pub fn MaintenanceHeld::is_delegated(&self) -> bool` (`matches!(self.how, How::Delegated)`).

- [ ] **Step 1: Write the failing test** — drive `ensure_targets_mounted` (or its `_with` core; find it with `grep -n 'fn ensure_targets_mounted' indexer/src/mount.rs`) with a probe saying the target is already mounted, once with a delegated hold and once with an owned (or the test `Assumed`) hold: delegated → an Info line `Target 'X': /m is mounted by the job that handed down the maintenance lock; left mounted for it`, and NO Warning; owned → the existing Warning text, unchanged. If the test module can only build `Assumed`, add a `#[cfg(test)] fn delegated_for_test(path) -> Self` constructor beside it.

- [ ] **Step 2: Run to see it fail**, then **Step 3: implement** — pass the hold's `is_delegated()` into the "already mounted" branch (both sites) and choose the level and words from it. The parent's own pre-existing-mount check (`backup-run.sh` and `MountGuard`) still reports a target an earlier run left mounted, which is why the child may be quiet.

- [ ] **Step 4: Run, Step 5: Falsify** (force `is_delegated` → `false`: the delegated case RED), **Step 6: Commit** — `Indexer under backup-run.sh: a target the parent mounted is not reported as left mounted (bd 8veh)`.

---

### Task 10: Docs, version, packaging, the whole gate

**Files:**
- Modify: `CMakeLists.txt` (`VERSION 0.7.23.0`), `indexer/Cargo.toml` (`version = "0.7.23.0"` or the crate's version form — check), every file under `packaging/` that carries the version, `CHANGELOG.md` (`[Unreleased]` → entries under the house style, `changelog.md` rule), `docs/btrdasd.1` (`backup boot-plan`; `backup boot-archive` exit codes; the boot step's incremental/full behaviour), `docs/ARCHITECTURE.md` (:143 — `BackupRun` with steps; the removed calls), `README.md` if it lists GUI operations, `.claude/rules/build.md` (the public-module count only if it changed: `grep -c '^pub mod ' indexer/src/lib.rs`)

- [ ] **Step 1:** CHANGELOG entries (Fixed: c4x inert boxes; woq boot step on incremental; dtm script ignoring `[boot]`; hyvh; 8veh. Changed: `BackupRun` signature; report always written; Rust boot failures fail the run; WARN tier. Removed: `BackupSnapshot`, `BackupSend`, `BackupBootArchive`).
- [ ] **Step 2:** The whole gate on the committed tree: fmt, clippy ×2, shellcheck, `cargo test` and `--features dbus` (record pass/fail totals), ctest with `-DREQUIRE_ALL_SHELL_CASES=ON` (18/18 or the new count), codespell, `man -l docs/btrdasd.1 >/dev/null`, markdownlint on changed docs.
- [ ] **Step 3:** Mutation gate on the branch diff (Global Constraints recipe); `mutants-gate.py` → `mutants OK`. Every surviving mutant is a missing test: add it, re-run.
- [ ] **Step 4:** `scrub-promo check origin/main..HEAD` → CLEAN.
- [ ] **Step 5: Commit** — `0.7.23.0: GUI operations honoured; one boot-step rule on both paths`.

---

## After the plan (controller, not a task)

1. Whole-branch review (Opus): the boot step deletes and replaces live subvolumes on targets — a safety-critical path.
2. Merge, CI green on the full SHA, install #N with the busy-check from `backup.md`, then `systemctl stop` the D-Bus helper so the next call activates the new one.
3. Operator GUI check: each box changes the job log (Snapshot only → no `resume` line; Send only → no `snapshot` line; Email unticked → `Report saved`, no mail; Boot Archive tooltip changes with the mode; Run grey with Snapshot and Send both unticked).
4. The first nightly after install: `boot_subvols` row present with the new words; `Next scheduled:` shows `unknown` or a date, never blank; no `already mounted before this run` warning lines.
5. Close c4x, woq, dtm, hyvh, 8veh with the evidence.
