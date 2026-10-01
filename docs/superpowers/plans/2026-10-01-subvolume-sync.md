# Subvolume Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every backup run adopts subvolumes that exist on a source volume and are not excluded, retires entries whose subvolume is gone, expires retired backups, and reports all of it, so that protecting a new subvolume needs no separate act.

**Architecture:** A pure planner (`adopt::plan_sync`) compares `config.toml` with per-volume subvolume listings and returns a plan; a pure `apply_plan` turns plan + config into a new config; a thin shell lists subvolumes through a command-runner seam, writes `config.toml` and `btrbk.conf` atomically, and prints a report. `backup-run.sh` calls `btrdasd subvol sync` after sources are verified and `btrdasd subvol expire` after btrbk. The btrbk.conf renderer moves from the binary-only `setup::templates` into the library so the library can regenerate it.

**Tech Stack:** Rust 2024 (`buttered_dasd` library + `btrdasd` binary), bash (`scripts/backup-run.sh`), btrbk, CMake/ctest for bash tests, cargo-mutants with `.github/scripts/mutants-gate.py`.

**Spec:** `docs/superpowers/specs/2026-10-01-subvolume-reconcile-design.md` — read it first. Task 1 amends it; where this plan and the spec differ after Task 1, that is a defect in the plan.

## Global Constraints

- **No new dependencies.** `tempfile` is a dev-dependency only; library code must not use it. Atomic writes use `std::fs` (write a sibling temp file, `sync_all`, `rename`).
- **Names.** The existing `buttered_dasd::reconcile` module and `btrdasd reconcile` command prune index rows and keep their names. New code lives in `indexer/src/adopt.rs`; the CLI is `btrdasd subvol sync` and `btrdasd subvol expire`.
- **No `pub` signature of existing library items changes**, except where a task says so explicitly.
- **Never run `cargo build` directly** — build through `cmake --build <dir>`. `cargo test`, `cargo clippy`, `cargo fmt` are fine. Use `export CARGO_TARGET_DIR=/tmp/das-backup-manager-target` for test/clippy runs; it must be **unset** for cargo-mutants.
- **Mutation gate.** Every Rust task ends with: `git diff --relative origin/main -- . > /tmp/sync.diff` from `indexer/`, then on a **copy** of the repo `env -u CARGO_TARGET_DIR cargo mutants --in-place --no-shuffle --in-diff /tmp/sync.diff` and `python3 ../.github/scripts/mutants-gate.py` → `OK`, 0 missed. Never `--in-place` in the real tree.
- **Never run `sudo`, `cmake --install`, `btrdasd setup`, `mount`, `btrfs` or `systemctl` on the host from a task** except Task 14, and there only after `systemctl is-active das-backup.service das-backup-full.service das-scrub.service` prints three `inactive`.
- **Fail-silent rule** (`.claude/rules/fail-silent.md`): a swallowed error must substitute the cautious value. A listing that fails or is empty means "unknown", never "nothing there".
- **Commits:** no AI/tool attribution anywhere. Message body ends with `bd: DAS-Backup-Manager-gte`. Run `~/.claude/bin/scrub-promo check origin/main..HEAD` before any push.
- **Dates** are `YYYY-MM-DD` strings in UTC. Retention windows are whole days: daily ×1, weekly ×7, monthly ×31, yearly ×366. Rounding up errs toward keeping.
- **Expiry rule (spec §9):** a retired series is kept on a target until `retired + longest window of that target`, then every snapshot of it on that target is deleted together. A target whose retention is all zero has no window: keep and report, never delete.
- Code style: match the surrounding file. Comments say why. `cargo fmt` and `cargo clippy --all-targets -- -D warnings` clean.

## Review Focus

The five conditions the spec implies that are most likely to hurt, each pinned by a named test in the task that owns the code:

1. **A subvolume path containing a space.** The old parser took the last whitespace field, so `media/My Films` was read as `Films` — sync would adopt a phantom and retire the real entry. Expected: the whole path is read. → Task 5, `parse_subvolume_paths_keeps_spaces`.
2. **A listing that succeeds but is empty, or a volume that is the wrong filesystem.** Expected: nothing on that volume is adopted, retired or revived, and the run is marked failed. → Task 5 `empty_listing_is_a_failed_listing`, Task 7 `wrong_uuid_volume_is_not_listed`.
3. **A subvolume renamed on disk.** It appears as one vanished entry plus one new subvolume. Expected: the old entry is retired (its backups age out) and the new name is adopted with a fresh full send; nothing is deleted at once. → Task 6, `rename_is_a_retire_plus_an_adopt`.
4. **A retired series whose name is a prefix of a live one** (`home` retired, `home-video` live). Expected: expiry deletes only `home.<timestamp>`, never `home-video.<timestamp>`. → Task 9, `series_snapshots_does_not_match_a_longer_name`.
5. **A target that is not attached when expiry runs.** Expected: nothing is deleted there, the entry is not removed from config, and it is reported as "not reachable" rather than as "no snapshots remain". → Task 9, `entry_is_kept_while_any_location_is_unreachable`.

## File Structure

| File | Status | Responsibility |
|---|---|---|
| `indexer/src/btrbk_conf.rs` | new (lib) | Render `btrbk.conf` text and resolve snapshot names. Moved out of `setup/templates.rs`. |
| `indexer/src/caldate.rs` | new (lib) | `YYYY-MM-DD` ↔ day number; today in UTC. No I/O except reading the clock. |
| `indexer/src/adopt.rs` | new (lib) | Exclusion matching, listing, `plan_sync`, `apply_plan`, `sync_subvolumes`, report text. |
| `indexer/src/expire.rs` | new (lib) | Retention window, series matching, expiry decision, `expire_retired`. |
| `indexer/src/fsutil.rs` | new (lib) | `write_atomic`, and the `CommandRunner` seam shared by `adopt`/`expire`. |
| `indexer/src/config.rs` | modify | `SubvolConfig.adopted/retired`, `[subvolumes] exclude`, duplicate-name validation, atomic `save`. |
| `indexer/src/doctor.rs` | modify | Uses `adopt` for listing/exclusion; heuristic and "suggested additions" removed; retired entries are not stale. |
| `indexer/src/subvol.rs` | modify | `SubvolConfig` literals use `..Default::default()`. |
| `indexer/src/setup/templates.rs` | modify | Re-exports the renderer from the lib. |
| `indexer/src/main.rs` | modify | `subvol sync`, `subvol expire`; `subvol add/remove/set-*` validate, save atomically and regenerate `btrbk.conf`; manual backup path calls sync. |
| `scripts/backup-run.sh` | modify | `load_config_env`, `sync_subvolumes`, `expire_retired_subvolumes`, report section. |
| `tests/test_subvol_sync.sh` | new | Bash tests of the three new shell functions, both directions. |
| `indexer/tests/subvol_sync_loopback.rs` | new | Root end-to-end on loopback BTRFS, with the counter-test. |
| `.github/workflows/mutants.yml` | modify | Weekly scope gains `adopt.rs`, `expire.rs`, `btrbk_conf.rs`, `caldate.rs`, `fsutil.rs`. |
| docs, rules, changelog | modify | Task 13. |

---

### Task 1: Amend the spec to match what the code allows

**Files:**
- Modify: `docs/superpowers/specs/2026-10-01-subvolume-reconcile-design.md`

**Interfaces:**
- Produces: the names every later task uses — module `adopt`, commands `subvol sync` / `subvol expire`.

- [ ] **Step 1: Apply these edits to the spec**

1. In §5.1's table and everywhere else, replace `reconcile_plan` → `adopt::plan_sync`, `apply_plan` → `adopt::apply_plan`, `run_reconcile` → `adopt::sync_subvolumes`, `expire_retired` → `expire::expire_retired`, and `btrdasd subvol reconcile` → `btrdasd subvol sync`. Add a row `btrdasd subvol expire`.
2. Directly under the §5.1 table add:

```markdown
The words "reconcile" and `btrdasd reconcile` already mean something else in
this codebase (pruning index rows for snapshots that no longer exist), so the
new step is called **sync** in code and CLI. This document's title keeps the
design name.
```

3. In §5.2 replace the sentence beginning "It runs under `--dryrun`" with:

```markdown
- Under `--dryrun` it runs as `subvol sync --dry-run`: the plan is printed,
  nothing is written.
- Expiry is a second command, `subvol expire`, because it needs the targets
  mounted and sync runs before they are.
```

4. In §5.3 replace "created by reconcile the first time it is needed, with `target_labels` set to the primary target's label and its own target subdirectory" with:

```markdown
created by sync the first time it is needed. It copies the volume, device and
snapshot directory of the first source declared for that volume, sets
`target_labels` to the primary target's label, and uses its own label as its
target subdirectory.
```

5. In §5.3 replace "if that collides with any name already in the config" with "if that collides with any snapshot name anywhere in the config".
6. In §5.5 after "then deleted together." add:

```markdown
Windows are counted in whole days: a daily tier counts as 1 day, weekly as 7,
monthly as 31, yearly as 366. The rounding is deliberate and errs toward
keeping. Dates are UTC.
```

7. In §5.6's table add a row:

```markdown
| A subvolume path contains a space | Read whole. The previous parser took the last word of the path; sync replaces it. |
```

8. In §5.2 add a bullet:

```markdown
- The Rust manual path verifies each source volume's filesystem UUID and that
  it is mounted at its top level before listing it. The bash path already did
  this in `verify_sources_before_write`; the Rust path did not.
```

- [ ] **Step 2: Lint and commit**

```bash
cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager
git ls-files -z -- . ':!.beads' | xargs -0 uvx codespell@2.4.3
git add docs/superpowers/specs/2026-10-01-subvolume-reconcile-design.md
git commit -m "Spec: name the new step 'sync' and record four code-driven details

'reconcile' already names the index-pruning command. Adds the day-count
rule for retention windows, the adoption source's fields, the UUID check
on the Rust path, and paths containing spaces.

bd: DAS-Backup-Manager-gte"
```

---

### Task 2: Move the btrbk.conf renderer into the library

`render_btrbk_conf` is `pub` but lives in `setup::templates`, which is compiled only into the binary. Library code cannot call it.

**Files:**
- Create: `indexer/src/btrbk_conf.rs`
- Modify: `indexer/src/lib.rs` (add `pub mod btrbk_conf;`)
- Modify: `indexer/src/setup/templates.rs:53-209` (delete the three functions, re-export)
- Test: tests move with the code (`templates.rs` tests at ~1022-1106 that call `resolve_snapshot_names`, `format_retention`, `render_btrbk_conf`)

**Interfaces:**
- Produces:
  - `pub fn buttered_dasd::btrbk_conf::render_btrbk_conf(config: &Config) -> String`
  - `pub fn buttered_dasd::btrbk_conf::resolve_snapshot_names(subvols: &[SubvolConfig]) -> Vec<String>`
  - `pub fn buttered_dasd::btrbk_conf::algorithmic_snapshot_name(subvol_name: &str) -> String`
  - `pub const buttered_dasd::btrbk_conf::GENERATED_HEADER: &str`

- [ ] **Step 1: Write the failing test** in a new `indexer/src/btrbk_conf.rs` containing only:

```rust
//! Rendering of `/etc/btrbk/btrbk.conf` from `config.toml`.
//!
//! Lives in the library, not in `setup/`, because the backup run itself
//! regenerates this file when it adopts or retires a subvolume (`adopt.rs`).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algorithmic_snapshot_name_strips_at_and_flattens_slashes() {
        assert_eq!(algorithmic_snapshot_name("@home"), "home");
        assert_eq!(algorithmic_snapshot_name("@srv/stremio-web"), "srv-stremio-web");
        assert_eq!(algorithmic_snapshot_name("bosco-media/video"), "bosco-media-video");
        // The bare top-level subvolume has nothing left once '@' is gone.
        assert_eq!(algorithmic_snapshot_name("@"), "root");
    }
}
```

and `pub mod btrbk_conf;` added to `indexer/src/lib.rs` (keep the list alphabetical: after `backup`).

- [ ] **Step 2: Run it and watch it fail**

Run: `cd indexer && cargo test --lib btrbk_conf`
Expected: FAIL to compile — `cannot find function algorithmic_snapshot_name`.

- [ ] **Step 3: Move the code**

Cut `GENERATED_HEADER` (templates.rs:12-14), `render_btrbk_conf` (53-137), `format_retention` (139-159) and `resolve_snapshot_names` (161-209) into `btrbk_conf.rs`, above the test module, with these changes only:

```rust
use crate::config::{Config, Retention, SubvolConfig, Target, TargetRole};

pub const GENERATED_HEADER: &str = /* the existing literal, unchanged */;

/// The snapshot name btrbk would be given for a subvolume with no explicit
/// `snapshot_name`: `@` removed, `/` flattened to `-`, and `root` for the
/// bare top-level subvolume.
pub fn algorithmic_snapshot_name(subvol_name: &str) -> String {
    let stripped = subvol_name.replace('@', "").replace('/', "-");
    if stripped.is_empty() {
        "root".to_string()
    } else {
        stripped
    }
}
```

Make `resolve_snapshot_names` `pub` and have both places that computed `sv.name.replace('@', "").replace('/', "-")` call `algorithmic_snapshot_name(&sv.name)` instead. Leave `render_btrbk_conf` and `format_retention` bodies exactly as they are (retired entries are handled in Task 3).

In `templates.rs`, replace the removed items with:

```rust
pub use buttered_dasd::btrbk_conf::{GENERATED_HEADER, render_btrbk_conf};
```

Move the tests that exercise the three moved functions into `btrbk_conf.rs`'s test module, together with any helper they need (`test_config()`, `subvol()`); tests that exercise other renderers stay in `templates.rs` and keep their own copy of `test_config()`.

- [ ] **Step 4: Run everything**

Run: `cd indexer && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS, and the total test count is unchanged plus one.

- [ ] **Step 5: Prove the rendered file did not change**

```bash
cd indexer && cargo test --lib render_btrbk_conf_renders_the_whole_file_for_a_multi_target_config
```
Expected: PASS — that test asserts the entire rendered file by equality.

- [ ] **Step 6: Mutation gate (see Global Constraints), then commit**

```bash
git add indexer/src/btrbk_conf.rs indexer/src/lib.rs indexer/src/setup/templates.rs
git commit -m "Move the btrbk.conf renderer into the library

The backup run will regenerate btrbk.conf when it adopts a subvolume,
and library code could not reach setup::templates.

bd: DAS-Backup-Manager-gte"
```

---

### Task 3: Config schema — adopted, retired, exclude list, atomic save, duplicate names

**Files:**
- Create: `indexer/src/fsutil.rs`
- Modify: `indexer/src/lib.rs` (`pub mod fsutil;`)
- Modify: `indexer/src/config.rs` — `SubvolConfig` (295-342), `Config` (12-33), `Doctor` (286-293), `save` (608-617), `validate` (621-689)
- Modify: `indexer/src/btrbk_conf.rs` — skip retired entries
- Modify: every `SubvolConfig { … }` literal (30 sites in `tests/boot_archive_loopback.rs`, `src/config.rs`, `src/backup.rs`, `src/doctor.rs`, `src/subvol.rs`, `src/setup/{templates,wizard,installer,env_export}.rs`)

**Interfaces:**
- Consumes: `btrbk_conf::resolve_snapshot_names`
- Produces:
  - `SubvolConfig { name, manual_only, snapshot_name, adopted: Option<String>, retired: Option<String> }`, `#[derive(Default)]`
  - `pub struct Subvolumes { pub exclude: Vec<String> }` at `Config.subvolumes`
  - `pub fn Config::exclude_patterns(&self) -> Vec<String>`
  - `pub fn fsutil::write_atomic(path: &Path, contents: &str) -> std::io::Result<()>`
  - `Config::save` writes atomically
  - `Config::validate()` reports duplicate snapshot names

- [ ] **Step 1: Write the failing tests** — append to `config.rs`'s `mod tests`:

```rust
    fn source_with(label: &str, volume: &str, subvols: &[(&str, Option<&str>)]) -> String {
        let mut s = format!(
            "[[source]]\nlabel = \"{label}\"\nvolume = \"{volume}\"\ndevice = \"/dev/sda\"\n"
        );
        for (name, snap) in subvols {
            s.push_str(&format!("[[source.subvolumes]]\nname = \"{name}\"\n"));
            if let Some(snap) = snap {
                s.push_str(&format!("snapshot_name = \"{snap}\"\n"));
            }
        }
        s
    }

    const ONE_TARGET: &str =
        "[[target]]\nlabel = \"t\"\nserial = \"X\"\nmount = \"/mnt/t\"\nrole = \"primary\"\n[target.retention]\ndaily = 7\n";

    #[test]
    fn adopted_and_retired_round_trip_and_are_absent_when_unset() {
        let extra = format!(
            "{}[[source.subvolumes]]\nname = \"b\"\nadopted = \"2026-10-02\"\nretired = \"2026-11-14\"\n{ONE_TARGET}",
            source_with("s", "/vol", &[("a", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        let a = &cfg.sources[0].subvolumes[0];
        let b = &cfg.sources[0].subvolumes[1];
        assert_eq!((a.adopted.as_deref(), a.retired.as_deref()), (None, None));
        assert_eq!(b.adopted.as_deref(), Some("2026-10-02"));
        assert_eq!(b.retired.as_deref(), Some("2026-11-14"));

        let text = cfg.to_toml().unwrap();
        assert_eq!(text.matches("adopted = ").count(), 1, "{text}");
        assert_eq!(text.matches("retired = ").count(), 1, "{text}");
        let again = Config::from_toml(&text).unwrap();
        assert_eq!(again.sources[0].subvolumes[1].retired.as_deref(), Some("2026-11-14"));
    }

    #[test]
    fn exclude_patterns_merge_the_old_doctor_key_and_the_defaults() {
        let extra = "[doctor]\nexclude = [\"@cache\", \"coredumps\"]\n\
                     [subvolumes]\nexclude = [\"scratch\", \"@cache\"]\n";
        let cfg = Config::from_toml(&minimal_toml(extra)).unwrap();
        // Order: built-in defaults, then [subvolumes], then [doctor]; no repeats.
        assert_eq!(
            cfg.exclude_patterns(),
            ["@tmp", "@var-tmp", "scratch", "@cache", "coredumps"]
        );
    }

    #[test]
    fn validate_rejects_two_entries_that_resolve_to_one_snapshot_name() {
        let extra = format!(
            "{}{ONE_TARGET}",
            source_with("s", "/vol", &[("a/b", None), ("a-b", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        let errors = cfg.validate();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("snapshot name 'a-b'")
                && errors[0].contains("'a/b'")
                && errors[0].contains("'a-b'"),
            "{errors:?}"
        );
    }

    #[test]
    fn validate_ignores_a_retired_entry_when_checking_snapshot_names() {
        let extra = format!(
            "{}[[source.subvolumes]]\nname = \"a-b\"\nretired = \"2026-01-01\"\n{ONE_TARGET}",
            source_with("s", "/vol", &[("a/b", None)])
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_checks_names_across_sources_sharing_a_snapshot_dir() {
        let extra = format!(
            "{}{}{ONE_TARGET}",
            source_with("one", "/vol", &[("x", Some("same"))]),
            source_with("two", "/vol", &[("y", Some("same"))]),
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        assert_eq!(cfg.validate().len(), 1, "{:?}", cfg.validate());
        // A different volume is a different snapshot directory.
        let extra = format!(
            "{}{}{ONE_TARGET}",
            source_with("one", "/vol", &[("x", Some("same"))]),
            source_with("two", "/other", &[("y", Some("same"))]),
        );
        let cfg = Config::from_toml(&minimal_toml(&extra)).unwrap();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn save_replaces_the_file_whole_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old contents that must not survive").unwrap();
        let cfg = Config::from_toml(&minimal_toml("")).unwrap();
        cfg.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Generated by btrdasd setup"));
        assert!(Config::from_toml(&text).is_ok());
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["config.toml"]);
    }
```

and in a new `indexer/src/fsutil.rs`:

```rust
//! Small filesystem and process helpers shared by the modules that change
//! the backup configuration during a run.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_refuses_when_the_directory_is_missing_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent/file.txt");
        assert!(write_atomic(&path, "x").is_err());
        assert!(!path.exists());
    }

    #[test]
    fn write_atomic_keeps_the_old_file_when_the_temp_file_cannot_be_created() {
        // A directory where the temp file would go makes creation fail.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "old").unwrap();
        std::fs::create_dir(dir.path().join(".f.tmp")).unwrap();
        assert!(write_atomic(&path, "new").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cd indexer && cargo test --lib config::tests fsutil`
Expected: FAIL to compile (`no field adopted`, `no method exclude_patterns`, `cannot find function write_atomic`).

- [ ] **Step 3: Implement**

`fsutil.rs`, above the tests:

```rust
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

/// Replace `path` with `contents` in one step: write a sibling temp file,
/// flush it to disk, then rename it over the target. A reader sees the old
/// file or the new one, never a truncated one — which matters because
/// `config.toml` and `btrbk.conf` are rewritten while a backup run is using
/// them.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    let result = (|| {
        let mut file = File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        // Best effort: the error being returned is the one that matters.
        let _ = fs::remove_file(&tmp);
    }
    result
}
```

`config.rs`:

```rust
#[derive(Debug, Clone, Default, Serialize)]
pub struct SubvolConfig {
    pub name: String,
    pub manual_only: bool,
    /// Override the btrbk snapshot_name (default: algorithmic from subvol name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_name: Option<String>,
    /// Date (`YYYY-MM-DD`, UTC) the backup run added this entry by itself.
    /// Absent on hand-written entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adopted: Option<String>,
    /// Date the subvolume was found to be gone. A retired entry is not sent
    /// to btrbk; its existing backups expire (`expire.rs`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired: Option<String>,
}
```

In the hand-written `Deserialize`, add to `SubvolEntry::Full`:

```rust
                #[serde(default)]
                adopted: Option<String>,
                #[serde(default)]
                retired: Option<String>,
```

and build with `SubvolConfig { name, ..Default::default() }` in the `Simple` arm and all five fields in the `Full` arm.

```rust
/// What may keep a subvolume out of the backup, apart from snapshot trees.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Subvolumes {
    /// Glob patterns (`*`, `?`) matched against the on-disk subvolume path.
    /// A pattern also excludes everything nested under what it matches.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// Always excluded: btrbk would snapshot scratch space on every run.
const DEFAULT_EXCLUDES: &[&str] = &["@tmp", "@var-tmp"];
```

Add `#[serde(default)] pub subvolumes: Subvolumes,` to `Config` after `doctor`, and:

```rust
impl Config {
    /// Every pattern that excludes a subvolume: the built-in defaults, then
    /// `[subvolumes].exclude`, then the older `[doctor].exclude`, without
    /// repeats. `[doctor].exclude` is still read so existing configs keep
    /// excluding what they excluded.
    pub fn exclude_patterns(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let all = DEFAULT_EXCLUDES
            .iter()
            .map(|s| s.to_string())
            .chain(self.subvolumes.exclude.iter().cloned())
            .chain(self.doctor.exclude.iter().cloned());
        for pattern in all {
            if !out.contains(&pattern) {
                out.push(pattern);
            }
        }
        out
    }
}
```

`save`: replace `fs::write(path, format!("{header}{body}"))?;` with `crate::fsutil::write_atomic(path, &format!("{header}{body}"))?;`.

`validate`, before `errors` is returned:

```rust
        // Two entries sharing a snapshot directory and a snapshot name would
        // overwrite each other's series; btrbk refuses the whole config.
        let mut seen: std::collections::HashMap<(String, String, String), String> =
            std::collections::HashMap::new();
        for src in &self.sources {
            let names = crate::btrbk_conf::resolve_snapshot_names(&src.subvolumes);
            for (sv, snap) in src.subvolumes.iter().zip(names) {
                if sv.retired.is_some() {
                    continue;
                }
                let key = (src.volume.clone(), src.snapshot_dir.clone(), snap.clone());
                if let Some(first) = seen.get(&key) {
                    errors.push(format!(
                        "Subvolumes '{first}' and '{}' on volume '{}' both resolve to \
                         snapshot name '{snap}' — give one an explicit snapshot_name",
                        sv.name, src.volume
                    ));
                } else {
                    seen.insert(key, sv.name.clone());
                }
            }
        }
```

`btrbk_conf.rs`, in the per-subvolume loop of `render_btrbk_conf`, skip retired entries and a source left with none:

```rust
        let live: Vec<(&SubvolConfig, &String)> = source
            .subvolumes
            .iter()
            .zip(snap_names.iter())
            .filter(|(sv, _)| sv.retired.is_none())
            .collect();
```

Compute `live` before the `# {label}` / `volume` lines are pushed and `continue` when it is empty, so a source whose entries are all retired renders no block at all. Add to `btrbk_conf.rs` tests:

```rust
    #[test]
    fn retired_entries_and_all_retired_sources_are_not_rendered() {
        let mut cfg = test_config();
        cfg.sources[0].subvolumes[0].retired = Some("2026-01-01".into());
        let text = render_btrbk_conf(&cfg);
        let gone = &cfg.sources[0].subvolumes[0].name;
        assert!(!text.contains(&format!("subvolume             {gone}\n")), "{text}");

        for sv in &mut cfg.sources[0].subvolumes {
            sv.retired = Some("2026-01-01".into());
        }
        let text = render_btrbk_conf(&cfg);
        assert!(!text.contains(&format!("# {}\n", cfg.sources[0].label)), "{text}");
    }
```

Then fix every `SubvolConfig { … }` literal the compiler reports by adding `..Default::default()`.

- [ ] **Step 4: Run everything**

Run: `cd indexer && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: PASS.

- [ ] **Step 5: Prove the live config still validates** (read-only; no sudo needed, the file is world-readable)

```bash
cd indexer && cargo run --quiet --bin btrdasd -- config validate --config /etc/das-backup/config.toml
```
Expected: `Config is valid.` If it reports a duplicate snapshot name, STOP and report it — do not weaken the check.

- [ ] **Step 6: Mutation gate, then commit**

```bash
git add -A indexer
git commit -m "Config: adopted/retired entries, one exclude list, atomic save, name collisions

- SubvolConfig gains optional adopted and retired dates; retired entries
  are not rendered into btrbk.conf
- [subvolumes].exclude, merged with the older [doctor].exclude and the
  built-in @tmp/@var-tmp
- Config::save writes a sibling temp file and renames it into place
- validate() rejects two live entries resolving to one snapshot name in
  the same snapshot directory

bd: DAS-Backup-Manager-gte"
```

---

### Task 4: Calendar days without a date library

**Files:**
- Create: `indexer/src/caldate.rs`
- Modify: `indexer/src/lib.rs` (`pub mod caldate;`)

**Interfaces:**
- Produces:
  - `pub fn caldate::day_number(date: &str) -> Option<i64>` — days since 1970-01-01 for a valid `YYYY-MM-DD`
  - `pub fn caldate::date_of(day: i64) -> String`
  - `pub fn caldate::today() -> String`

- [ ] **Step 1: Write the failing tests** (`indexer/src/caldate.rs`)

```rust
//! Whole-day calendar arithmetic on `YYYY-MM-DD` strings, UTC.
//!
//! Retirement and expiry only ever ask "how many days apart are these two
//! dates", so this is the proleptic Gregorian day count and nothing more.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_number_counts_from_the_unix_epoch() {
        assert_eq!(day_number("1970-01-01"), Some(0));
        assert_eq!(day_number("1970-01-02"), Some(1));
        assert_eq!(day_number("1969-12-31"), Some(-1));
        // 2026-10-01 is 20727 days after the epoch (`date -ud 2026-10-01 +%s` / 86400).
        assert_eq!(day_number("2026-10-01"), Some(20727));
    }

    #[test]
    fn day_number_knows_leap_years() {
        assert_eq!(
            day_number("2024-03-01").unwrap() - day_number("2024-02-28").unwrap(),
            2
        );
        assert_eq!(
            day_number("2026-03-01").unwrap() - day_number("2026-02-28").unwrap(),
            1
        );
        // Century rule: 2100 is not a leap year, 2000 was.
        assert_eq!(day_number("2100-02-29"), None);
        assert!(day_number("2000-02-29").is_some());
    }

    #[test]
    fn day_number_rejects_anything_that_is_not_a_real_date() {
        for bad in [
            "", "2026-10", "2026-13-01", "2026-00-10", "2026-04-31", "2026-10-00",
            "26-10-01", "2026/10/01", "2026-10-01T00:00", " 2026-10-01", "2026-1-1",
            "abcd-ef-gh",
        ] {
            assert_eq!(day_number(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn date_of_is_the_inverse_of_day_number() {
        for date in ["1970-01-01", "1999-12-31", "2000-02-29", "2026-10-01", "2027-01-01"] {
            assert_eq!(date_of(day_number(date).unwrap()), date);
        }
        assert_eq!(date_of(20727 + 7), "2026-10-08");
    }

    #[test]
    fn today_is_a_valid_date_not_before_this_code_was_written() {
        let today = today();
        assert!(day_number(&today).unwrap() >= day_number("2026-10-01").unwrap(), "{today}");
    }
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cd indexer && cargo test --lib caldate`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** (above the tests)

```rust
use std::time::{SystemTime, UNIX_EPOCH};

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 for a `YYYY-MM-DD` date, or `None` if the text is
/// not exactly that shape or is not a real calendar date.
pub fn day_number(date: &str) -> Option<i64> {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let digits = |s: &str| -> Option<i64> {
        s.bytes().all(|b| b.is_ascii_digit()).then(|| s.parse().ok())?
    };
    let (year, month, day) = (digits(&date[0..4])?, digits(&date[5..7])?, digits(&date[8..10])?);
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    // Days-from-civil (H. Hinnant): count in 400-year eras with the year
    // starting in March, so the leap day is the last day of the year.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y - era * 400;
    let month_from_march = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

/// The `YYYY-MM-DD` date `day` days after 1970-01-01.
pub fn date_of(day: i64) -> String {
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let d = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let m = if month_from_march < 10 { month_from_march + 3 } else { month_from_march - 9 };
    let y = year_of_era + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today's date in UTC.
pub fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        // A clock before 1970 is a broken clock; day 0 makes every retired
        // series look freshly retired, so nothing is expired on its account.
        .unwrap_or(0);
    date_of(secs.div_euclid(86_400))
}
```

- [ ] **Step 4: Run** `cd indexer && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test --lib caldate` → PASS.

- [ ] **Step 5: Mutation gate, then commit**

```bash
git add indexer/src/caldate.rs indexer/src/lib.rs
git commit -m "Add whole-day calendar arithmetic for retirement and expiry dates

bd: DAS-Backup-Manager-gte"
```

---

### Task 5: Exclusion matching and listing

**Files:**
- Create: `indexer/src/adopt.rs`
- Modify: `indexer/src/lib.rs` (`pub mod adopt;`)
- Modify: `indexer/src/fsutil.rs` (the `CommandRunner` seam)

**Interfaces:**
- Consumes: `doctor::glob_match(pattern: &str, text: &str) -> bool` (stays where it is; `forget.rs` imports it)
- Produces:
  - `pub trait fsutil::CommandRunner { fn output(&self, cmd: &mut Command) -> io::Result<Output>; }`, `pub struct fsutil::SystemRunner;`
  - `pub fn adopt::in_snapshot_tree(path: &str) -> bool`
  - `pub fn adopt::excluding_pattern<'a>(path: &str, patterns: &'a [String]) -> Option<&'a str>`
  - `pub fn adopt::parse_subvolume_paths(stdout: &str) -> Vec<String>`
  - `pub struct adopt::VolumeListing { pub volume: String, pub subvolumes: Result<Vec<String>, String> }`
  - `pub fn adopt::normalize_listing(volume: &str, listed: Result<Vec<String>, String>) -> VolumeListing`

- [ ] **Step 1: Write the failing tests** (`indexer/src/adopt.rs`)

```rust
//! Keep `config.toml` in step with the subvolumes that actually exist.
//!
//! The backup used to be an allowlist: a subvolume was backed up only if an
//! entry named it, and `btrfs send` does not descend into nested subvolumes,
//! so a missing entry meant a silently empty directory in every snapshot.
//! This module inverts that. See
//! `docs/superpowers/specs/2026-10-01-subvolume-reconcile-design.md`.

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn in_snapshot_tree_matches_a_snapshot_directory_at_any_depth() {
        for inside in [
            ".snapshots",
            ".snapshots/4460/snapshot",
            "@root/.snapshots/3811/snapshot",
            ".btrbk-snapshots/root.20260801T0325",
            "Audiobooks/.btrbk-snapshots/foo.20260101T0000",
        ] {
            assert!(in_snapshot_tree(inside), "{inside}");
        }
        for outside in ["@", "@home", "snapshots", "my.snapshots", "a/.snapshotsx/b"] {
            assert!(!in_snapshot_tree(outside), "{outside}");
        }
    }

    #[test]
    fn excluding_pattern_covers_the_path_and_everything_nested_under_it() {
        let p = pats(&["@cache", "coredumps", "scratch-*"]);
        assert_eq!(excluding_pattern("@cache", &p), Some("@cache"));
        assert_eq!(excluding_pattern("@cache/stremio", &p), Some("@cache"));
        assert_eq!(excluding_pattern("@cache/a/b/c", &p), Some("@cache"));
        assert_eq!(excluding_pattern("scratch-1/tmp", &p), Some("scratch-*"));
        // A sibling that merely starts with the same letters is not nested.
        assert_eq!(excluding_pattern("@cache2", &p), None);
        assert_eq!(excluding_pattern("@cachefoo/x", &p), None);
        // Nesting is downward only: a pattern for a child does not exclude its parent.
        assert_eq!(excluding_pattern("a", &pats(&["a/b"])), None);
        assert_eq!(excluding_pattern("@home", &p), None);
        assert_eq!(excluding_pattern("@home", &[]), None);
    }

    #[test]
    fn excluding_pattern_reports_the_first_pattern_that_applies() {
        let p = pats(&["zzz", "@cache*", "@cache"]);
        assert_eq!(excluding_pattern("@cache/x", &p), Some("@cache*"));
    }

    #[test]
    fn parse_subvolume_paths_keeps_spaces() {
        let out = "ID 256 gen 100 top level 5 path @\n\
                   ID 300 gen 9 top level 257 path bosco-media/My Films\n\
                   ID 301 gen 9 top level 5 path a path b\n\
                   \n\
                   garbage line with no marker\n";
        assert_eq!(
            parse_subvolume_paths(out),
            ["@", "bosco-media/My Films", "a path b"]
        );
    }

    #[test]
    fn empty_listing_is_a_failed_listing() {
        let l = normalize_listing("/vol", Ok(Vec::new()));
        assert_eq!(l.volume, "/vol");
        let why = l.subvolumes.unwrap_err();
        assert!(why.contains("no subvolumes"), "{why}");

        let l = normalize_listing("/vol", Err("boom".into()));
        assert_eq!(l.subvolumes.unwrap_err(), "boom");

        let l = normalize_listing("/vol", Ok(vec!["@".into()]));
        assert_eq!(l.subvolumes.unwrap(), ["@"]);
    }
}
```

and in `fsutil.rs` tests:

```rust
    #[test]
    fn system_runner_captures_output_and_exit_status() {
        let out = SystemRunner
            .output(std::process::Command::new("sh").args(["-c", "printf hi; exit 3"]))
            .unwrap();
        assert_eq!(out.stdout, b"hi");
        assert_eq!(out.status.code(), Some(3));
        assert!(
            SystemRunner
                .output(&mut std::process::Command::new("/nonexistent/das-no-such-binary"))
                .is_err()
        );
    }
```

- [ ] **Step 2: Run and watch them fail** — `cd indexer && cargo test --lib adopt fsutil` → FAIL to compile.

- [ ] **Step 3: Implement**

`fsutil.rs`:

```rust
use std::process::{Command, Output};

/// Every external command `adopt` and `expire` run goes through this, so
/// tests can script what `btrfs`, `findmnt` and `blkid` answer.
pub trait CommandRunner {
    fn output(&self, cmd: &mut Command) -> io::Result<Output>;
}

/// The real thing.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn output(&self, cmd: &mut Command) -> io::Result<Output> {
        cmd.output()
    }
}
```

`adopt.rs`, above the tests:

```rust
use crate::doctor::glob_match;

/// Whether `path` lies in a snapshot tree — Snapper's `.snapshots` or btrbk's
/// `.btrbk-snapshots`, at any depth. Snapshots are not data to back up; they
/// are copies of data that is.
pub fn in_snapshot_tree(path: &str) -> bool {
    path.split('/')
        .any(|component| component == ".snapshots" || component == ".btrbk-snapshots")
}

/// The first exclude pattern that keeps `path` out, if any. A pattern applies
/// to the subvolume it matches and to everything nested under that
/// subvolume, so excluding `@cache` also excludes `@cache/stremio`.
pub fn excluding_pattern<'a>(path: &str, patterns: &'a [String]) -> Option<&'a str> {
    patterns
        .iter()
        .find(|pattern| {
            // The path itself, then each ancestor: "a/b/c", "a/b", "a".
            let mut candidate = path;
            loop {
                if glob_match(pattern, candidate) {
                    return true;
                }
                match candidate.rfind('/') {
                    Some(cut) => candidate = &candidate[..cut],
                    None => return false,
                }
            }
        })
        .map(String::as_str)
}

/// Subvolume paths from `btrfs subvolume list` output. Each line reads
/// `ID 256 gen 100 top level 5 path <path>`; the path is everything after
/// the first ` path `, because a path may itself contain spaces.
pub fn parse_subvolume_paths(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_once(" path "))
        .map(|(_, path)| path.to_string())
        .filter(|path| !path.is_empty())
        .collect()
}

/// What was found on one source volume. `Err` means the volume could not be
/// read and nothing may be concluded about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeListing {
    pub volume: String,
    pub subvolumes: Result<Vec<String>, String>,
}

/// An empty listing is treated as a failed one. Every configured volume holds
/// at least the subvolumes that are being backed up from it, so "nothing
/// here" is the signature of reading the wrong filesystem, and believing it
/// would retire every entry on the volume.
pub fn normalize_listing(volume: &str, listed: Result<Vec<String>, String>) -> VolumeListing {
    let subvolumes = match listed {
        Ok(found) if found.is_empty() => Err(format!(
            "'{volume}' listed no subvolumes — treating the volume as unreadable"
        )),
        other => other,
    };
    VolumeListing {
        volume: volume.to_string(),
        subvolumes,
    }
}
```

- [ ] **Step 4: Run** `cd indexer && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` → PASS.

- [ ] **Step 5: Mutation gate, then commit**

```bash
git add indexer/src/adopt.rs indexer/src/fsutil.rs indexer/src/lib.rs
git commit -m "adopt: exclusion that covers nested paths, and a listing parser that keeps spaces

An exclude pattern now also excludes everything under what it matches.
The parser reads the whole path after ' path '; the previous one took the
last word, which misread any subvolume whose path contains a space.

bd: DAS-Backup-Manager-gte"
```

---

### Task 6: The planner and apply

**Files:**
- Modify: `indexer/src/adopt.rs`

**Interfaces:**
- Consumes: `Config`, `Source`, `SubvolConfig`, `TargetRole`, `Config::exclude_patterns()`, `btrbk_conf::{algorithmic_snapshot_name, resolve_snapshot_names}`, `VolumeListing`, `in_snapshot_tree`, `excluding_pattern`
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason { SnapshotTree, Excluded { pattern: String } }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption { pub volume: String, pub name: String, pub source_label: String, pub nested_under: Option<String>, pub manual_only: bool }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRef { pub source_label: String, pub name: String }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip { pub volume: String, pub name: String, pub reason: SkipReason }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unplaceable { pub volume: String, pub name: String, pub why: String }
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub adopt: Vec<Adoption>, pub retire: Vec<EntryRef>, pub revive: Vec<EntryRef>,
    pub skipped: Vec<Skip>, pub unplaceable: Vec<Unplaceable>, pub failed_volumes: Vec<(String, String)>,
}
impl SyncPlan { pub fn changes_config(&self) -> bool; pub fn failed(&self) -> bool; }
pub fn adoption_source_label(config: &Config, volume: &str) -> Option<String>
pub fn plan_sync(config: &Config, listings: &[VolumeListing]) -> SyncPlan
pub fn apply_plan(config: &Config, plan: &SyncPlan, today: &str) -> Config
```

- [ ] **Step 1: Write the failing tests** — add to `adopt.rs` tests:

```rust
    use crate::config::{Config, Retention, Source, SubvolConfig, Target, TargetRole};

    fn sv(name: &str) -> SubvolConfig {
        SubvolConfig { name: name.into(), ..Default::default() }
    }

    fn source(label: &str, volume: &str, targets: &[&str], subvols: &[&str]) -> Source {
        Source {
            label: label.into(),
            volume: volume.into(),
            subvolumes: subvols.iter().map(|n| sv(n)).collect(),
            device: format!("UUID=uuid-of-{volume}"),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![label.into()],
            target_labels: targets.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn target(label: &str, role: TargetRole) -> Target {
        Target {
            label: label.into(),
            serial: "S".into(),
            serials: vec!["S".into()],
            mount_uuid: None,
            mount: format!("/mnt/{label}"),
            role,
            retention: Retention { daily: 7, ..Default::default() },
            display_name: String::new(),
        }
    }

    /// Two volumes. `/ssd` has two sources (one to everything, one to the
    /// primary only); `/hdd` has one.
    fn config() -> Config {
        let mut c = Config::default();
        c.targets = vec![target("big", TargetRole::Primary), target("small", TargetRole::Mirror)];
        c.sources = vec![
            source("ssd", "/ssd", &[], &["@srv", "@opt"]),
            source("ssd-vm", "/ssd", &["big"], &["@srv/VirtualMachines"]),
            source("media", "/hdd", &["big"], &["bosco-media"]),
        ];
        c.doctor.exclude = vec!["@cache".into()];
        c
    }

    fn listing(volume: &str, names: &[&str]) -> VolumeListing {
        VolumeListing {
            volume: volume.into(),
            subvolumes: Ok(names.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn all_present() -> Vec<VolumeListing> {
        vec![
            listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines"]),
            listing("/hdd", &["bosco-media"]),
        ]
    }

    #[test]
    fn nothing_to_do_when_disk_and_config_agree() {
        let plan = plan_sync(&config(), &all_present());
        assert_eq!(plan, SyncPlan::default());
        assert!(!plan.changes_config());
        assert!(!plan.failed());
    }

    #[test]
    fn nested_subvolume_joins_its_nearest_configured_ancestors_source() {
        let mut l = all_present();
        l[0] = listing(
            "/ssd",
            &["@srv", "@opt", "@srv/VirtualMachines", "@srv/stremio-web", "@srv/VirtualMachines/win11"],
        );
        l[1] = listing("/hdd", &["bosco-media", "bosco-media/video"]);
        let plan = plan_sync(&config(), &l);
        assert_eq!(
            plan.adopt,
            vec![
                Adoption {
                    volume: "/ssd".into(),
                    name: "@srv/VirtualMachines/win11".into(),
                    // Two ancestors are configured; the nearer one wins.
                    source_label: "ssd-vm".into(),
                    nested_under: Some("@srv/VirtualMachines".into()),
                    manual_only: false,
                },
                Adoption {
                    volume: "/ssd".into(),
                    name: "@srv/stremio-web".into(),
                    source_label: "ssd".into(),
                    nested_under: Some("@srv".into()),
                    manual_only: false,
                },
                Adoption {
                    volume: "/hdd".into(),
                    name: "bosco-media/video".into(),
                    source_label: "media".into(),
                    nested_under: Some("bosco-media".into()),
                    manual_only: false,
                },
            ]
        );
        assert!(plan.changes_config());
    }

    #[test]
    fn a_name_that_only_shares_a_prefix_is_not_nested() {
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines", "@srv2", "@optional"]);
        let plan = plan_sync(&config(), &l);
        let names: Vec<_> = plan.adopt.iter().map(|a| (a.name.as_str(), a.nested_under.clone())).collect();
        assert_eq!(names, [("@optional", None), ("@srv2", None)]);
        assert!(plan.adopt.iter().all(|a| a.source_label == "ssd-adopted"));
    }

    #[test]
    fn nested_subvolume_inherits_manual_only() {
        let mut c = config();
        c.sources[0].subvolumes[0].manual_only = true; // @srv
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines", "@srv/x"]);
        assert!(plan_sync(&c, &l).adopt[0].manual_only);
    }

    #[test]
    fn excluded_and_snapshot_tree_subvolumes_are_skipped_with_the_reason() {
        let mut l = all_present();
        l[0] = listing(
            "/ssd",
            &["@srv", "@opt", "@srv/VirtualMachines", "@cache", "@cache/stremio", ".btrbk-snapshots/opt.20261001T0300", "@tmp"],
        );
        let plan = plan_sync(&config(), &l);
        assert!(plan.adopt.is_empty(), "{:?}", plan.adopt);
        assert_eq!(
            plan.skipped,
            vec![
                Skip { volume: "/ssd".into(), name: ".btrbk-snapshots/opt.20261001T0300".into(), reason: SkipReason::SnapshotTree },
                Skip { volume: "/ssd".into(), name: "@cache".into(), reason: SkipReason::Excluded { pattern: "@cache".into() } },
                Skip { volume: "/ssd".into(), name: "@cache/stremio".into(), reason: SkipReason::Excluded { pattern: "@cache".into() } },
                Skip { volume: "/ssd".into(), name: "@tmp".into(), reason: SkipReason::Excluded { pattern: "@tmp".into() } },
            ]
        );
        assert!(!plan.changes_config());
    }

    #[test]
    fn a_configured_subvolume_is_never_skipped_even_if_a_pattern_matches_it() {
        let mut c = config();
        c.subvolumes.exclude = vec!["@opt".into()];
        let plan = plan_sync(&c, &all_present());
        assert_eq!(plan, SyncPlan::default());
    }

    #[test]
    fn vanished_subvolume_is_retired_and_a_returning_one_is_revived() {
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@srv/VirtualMachines"]); // @opt gone
        let plan = plan_sync(&config(), &l);
        assert_eq!(plan.retire, vec![EntryRef { source_label: "ssd".into(), name: "@opt".into() }]);
        assert!(plan.revive.is_empty());

        let mut c = config();
        c.sources[0].subvolumes[1].retired = Some("2026-09-01".into()); // @opt
        // Still gone: already retired, nothing to do.
        assert_eq!(plan_sync(&c, &l), SyncPlan::default());
        // Back again: revived, and not adopted a second time.
        let plan = plan_sync(&c, &all_present());
        assert_eq!(plan.revive, vec![EntryRef { source_label: "ssd".into(), name: "@opt".into() }]);
        assert!(plan.adopt.is_empty());
    }

    #[test]
    fn rename_is_a_retire_plus_an_adopt() {
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@opt-new", "@srv/VirtualMachines"]);
        let plan = plan_sync(&config(), &l);
        assert_eq!(plan.retire, vec![EntryRef { source_label: "ssd".into(), name: "@opt".into() }]);
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].name, "@opt-new");
    }

    #[test]
    fn a_retired_ancestor_does_not_place_a_child() {
        let mut c = config();
        c.sources[0].subvolumes[0].retired = Some("2026-09-01".into()); // @srv
        let l = vec![listing("/ssd", &["@opt", "@srv/VirtualMachines", "@srv/x"]), listing("/hdd", &["bosco-media"])];
        let plan = plan_sync(&c, &l);
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].nested_under, None);
        assert_eq!(plan.adopt[0].source_label, "ssd-adopted");
    }

    #[test]
    fn a_failed_volume_changes_nothing_on_that_volume_only() {
        let l = vec![
            VolumeListing { volume: "/ssd".into(), subvolumes: Err("not mounted".into()) },
            listing("/hdd", &["bosco-media", "bosco-media/video"]),
        ];
        let plan = plan_sync(&config(), &l);
        assert_eq!(plan.failed_volumes, vec![("/ssd".into(), "not mounted".into())]);
        assert!(plan.retire.is_empty(), "nothing on /ssd may be retired: {:?}", plan.retire);
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].volume, "/hdd");
        assert!(plan.failed());
    }

    #[test]
    fn a_volume_with_no_listing_at_all_is_a_failed_volume() {
        let plan = plan_sync(&config(), &[listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines"])]);
        assert_eq!(plan.failed_volumes.len(), 1);
        assert_eq!(plan.failed_volumes[0].0, "/hdd");
        assert!(plan.retire.is_empty());
    }

    #[test]
    fn top_level_subvolume_without_a_primary_target_is_unplaceable() {
        let mut c = config();
        c.targets.retain(|t| t.role != TargetRole::Primary);
        let mut l = all_present();
        l[1] = listing("/hdd", &["bosco-media", "new-top", "bosco-media/video"]);
        let plan = plan_sync(&c, &l);
        assert_eq!(plan.unplaceable.len(), 1);
        assert_eq!(plan.unplaceable[0].name, "new-top");
        assert!(plan.unplaceable[0].why.contains("primary"), "{}", plan.unplaceable[0].why);
        // The nested one still has a home.
        assert_eq!(plan.adopt.len(), 1);
        assert!(plan.failed());
    }

    #[test]
    fn apply_adds_entries_with_explicit_unique_names_and_the_date() {
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines", "@srv/stremio-web", "opt", "@new"]);
        let c = config();
        let plan = plan_sync(&c, &l);
        let out = apply_plan(&c, &plan, "2026-10-02");

        let ssd = &out.sources[0];
        let web = ssd.subvolumes.iter().find(|s| s.name == "@srv/stremio-web").unwrap();
        assert_eq!(web.snapshot_name.as_deref(), Some("srv-stremio-web"));
        assert_eq!(web.adopted.as_deref(), Some("2026-10-02"));

        let adopted = out.sources.iter().find(|s| s.label == "ssd-adopted").unwrap();
        assert_eq!(adopted.volume, "/ssd");
        assert_eq!(adopted.device, "UUID=uuid-of-/ssd");
        assert_eq!(adopted.snapshot_dir, ".btrbk-snapshots");
        assert_eq!(adopted.target_labels, ["big"]);
        assert_eq!(adopted.target_subdirs, ["ssd-adopted"]);
        let names: Vec<_> = adopted
            .subvolumes
            .iter()
            .map(|s| (s.name.as_str(), s.snapshot_name.as_deref().unwrap()))
            .collect();
        // "@opt" already owns the name "opt", so the bare "opt" gets "opt-2".
        assert_eq!(names, [("@new", "new"), ("opt", "opt-2")]);
        assert!(out.validate().is_empty(), "{:?}", out.validate());
        // The input is untouched.
        assert_eq!(c.sources.len(), 3);
    }

    #[test]
    fn apply_reuses_an_existing_adoption_source() {
        let c = config();
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines", "@one"]);
        let once = apply_plan(&c, &plan_sync(&c, &l), "2026-10-02");
        l[0] = listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines", "@one", "@two"]);
        let twice = apply_plan(&once, &plan_sync(&once, &l), "2026-10-03");
        assert_eq!(twice.sources.iter().filter(|s| s.label == "ssd-adopted").count(), 1);
        let adopted = twice.sources.iter().find(|s| s.label == "ssd-adopted").unwrap();
        assert_eq!(adopted.subvolumes.len(), 2);
        // Running the planner on its own result finds nothing more to do.
        assert_eq!(plan_sync(&twice, &l), SyncPlan::default());
    }

    #[test]
    fn apply_stamps_retirement_and_clears_it_on_revival() {
        let c = config();
        let gone = vec![listing("/ssd", &["@srv", "@srv/VirtualMachines"]), listing("/hdd", &["bosco-media"])];
        let retired = apply_plan(&c, &plan_sync(&c, &gone), "2026-10-02");
        let opt = retired.sources[0].subvolumes.iter().find(|s| s.name == "@opt").unwrap();
        assert_eq!(opt.retired.as_deref(), Some("2026-10-02"));

        let back = apply_plan(&retired, &plan_sync(&retired, &all_present()), "2026-10-09");
        let opt = back.sources[0].subvolumes.iter().find(|s| s.name == "@opt").unwrap();
        assert_eq!(opt.retired, None);
    }
```

- [ ] **Step 2: Run and watch them fail** — `cd indexer && cargo test --lib adopt` → FAIL to compile.

- [ ] **Step 3: Implement** (in `adopt.rs`, after `normalize_listing`)

```rust
use std::collections::HashSet;

use crate::btrbk_conf::{algorithmic_snapshot_name, resolve_snapshot_names};
use crate::config::{Config, Source, SubvolConfig, TargetRole};

// (the struct and enum definitions exactly as in this task's Interfaces block)

impl SyncPlan {
    /// Whether applying the plan would alter `config.toml`.
    pub fn changes_config(&self) -> bool {
        !(self.adopt.is_empty() && self.retire.is_empty() && self.revive.is_empty())
    }

    /// Whether the run must be reported as failed: something could not be
    /// read, or something that should be backed up could not be placed.
    pub fn failed(&self) -> bool {
        !(self.failed_volumes.is_empty() && self.unplaceable.is_empty())
    }
}

/// The label of the source that holds adopted subvolumes with no configured
/// ancestor on `volume`: the first source declared for that volume, with
/// `-adopted` appended. `None` if no source uses the volume.
pub fn adoption_source_label(config: &Config, volume: &str) -> Option<String> {
    config
        .sources
        .iter()
        .find(|s| s.volume == volume && !s.label.ends_with("-adopted"))
        .map(|s| format!("{}-adopted", s.label))
}

fn is_nested_under(child: &str, parent: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Compare the config with what was found on each source volume.
pub fn plan_sync(config: &Config, listings: &[VolumeListing]) -> SyncPlan {
    let mut plan = SyncPlan::default();
    let patterns = config.exclude_patterns();
    let has_primary = config.targets.iter().any(|t| t.role == TargetRole::Primary);

    let mut volumes: Vec<&str> = Vec::new();
    for source in &config.sources {
        if !volumes.contains(&source.volume.as_str()) {
            volumes.push(&source.volume);
        }
    }

    for volume in volumes {
        let on_disk = match listings.iter().find(|l| l.volume == volume) {
            Some(VolumeListing { subvolumes: Ok(found), .. }) => found,
            Some(VolumeListing { subvolumes: Err(why), .. }) => {
                plan.failed_volumes.push((volume.to_string(), why.clone()));
                continue;
            }
            None => {
                plan.failed_volumes
                    .push((volume.to_string(), "volume was not listed".to_string()));
                continue;
            }
        };
        let sources: Vec<&Source> = config.sources.iter().filter(|s| s.volume == volume).collect();

        for source in &sources {
            for entry in &source.subvolumes {
                let present = on_disk.contains(&entry.name);
                let reference = EntryRef {
                    source_label: source.label.clone(),
                    name: entry.name.clone(),
                };
                match (present, entry.retired.is_some()) {
                    (false, false) => plan.retire.push(reference),
                    (true, true) => plan.revive.push(reference),
                    _ => {}
                }
            }
        }

        let mut unknown: Vec<&String> = on_disk
            .iter()
            .filter(|name| {
                !sources
                    .iter()
                    .any(|s| s.subvolumes.iter().any(|e| &e.name == *name))
            })
            .collect();
        unknown.sort();
        unknown.dedup();

        for name in unknown {
            if in_snapshot_tree(name) {
                plan.skipped.push(Skip {
                    volume: volume.to_string(),
                    name: name.clone(),
                    reason: SkipReason::SnapshotTree,
                });
                continue;
            }
            if let Some(pattern) = excluding_pattern(name, &patterns) {
                plan.skipped.push(Skip {
                    volume: volume.to_string(),
                    name: name.clone(),
                    reason: SkipReason::Excluded { pattern: pattern.to_string() },
                });
                continue;
            }
            // The nearest configured, live ancestor: the longest name that is
            // a path prefix. A retired ancestor is no longer backed up, so it
            // cannot lend its scoping.
            let ancestor = sources
                .iter()
                .flat_map(|s| s.subvolumes.iter().map(move |e| (*s, e)))
                .filter(|(_, e)| e.retired.is_none() && is_nested_under(name, &e.name))
                .max_by_key(|(_, e)| e.name.len());

            match ancestor {
                Some((source, entry)) => plan.adopt.push(Adoption {
                    volume: volume.to_string(),
                    name: name.clone(),
                    source_label: source.label.clone(),
                    nested_under: Some(entry.name.clone()),
                    manual_only: entry.manual_only,
                }),
                None if has_primary => plan.adopt.push(Adoption {
                    volume: volume.to_string(),
                    name: name.clone(),
                    // `sources` is non-empty: the volume came from a source.
                    source_label: adoption_source_label(config, volume)
                        .unwrap_or_else(|| format!("{}-adopted", sources[0].label)),
                    nested_under: None,
                    manual_only: false,
                }),
                None => plan.unplaceable.push(Unplaceable {
                    volume: volume.to_string(),
                    name: name.clone(),
                    why: "no target has the primary role, and a subvolume with no \
                          configured parent is sent to the primary only"
                        .to_string(),
                }),
            }
        }
    }
    plan
}

/// `base`, or `base-2`, `base-3`, … — whichever is not in `taken`.
fn unique_name(base: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("an unbounded range always yields an unused name")
}

/// The config that results from carrying out `plan` on `today`.
pub fn apply_plan(config: &Config, plan: &SyncPlan, today: &str) -> Config {
    let mut out = config.clone();

    for reference in &plan.retire {
        if let Some(entry) = entry_mut(&mut out, reference) {
            entry.retired = Some(today.to_string());
        }
    }
    for reference in &plan.revive {
        if let Some(entry) = entry_mut(&mut out, reference) {
            entry.retired = None;
        }
    }

    // Every snapshot name in use, so an adopted entry can never collide with
    // one. Existing names are never changed.
    let mut taken: HashSet<String> = out
        .sources
        .iter()
        .flat_map(|s| resolve_snapshot_names(&s.subvolumes))
        .collect();

    for adoption in &plan.adopt {
        if !out.sources.iter().any(|s| s.label == adoption.source_label) {
            let Some(model) = config.sources.iter().find(|s| s.volume == adoption.volume) else {
                continue;
            };
            let primary: Vec<String> = config
                .targets
                .iter()
                .filter(|t| t.role == TargetRole::Primary)
                .map(|t| t.label.clone())
                .take(1)
                .collect();
            out.sources.push(Source {
                label: adoption.source_label.clone(),
                volume: model.volume.clone(),
                subvolumes: Vec::new(),
                device: model.device.clone(),
                snapshot_dir: model.snapshot_dir.clone(),
                target_subdirs: vec![adoption.source_label.clone()],
                target_labels: primary,
            });
        }
        let snapshot_name = unique_name(&algorithmic_snapshot_name(&adoption.name), &taken);
        taken.insert(snapshot_name.clone());
        if let Some(source) = out.sources.iter_mut().find(|s| s.label == adoption.source_label) {
            source.subvolumes.push(SubvolConfig {
                name: adoption.name.clone(),
                manual_only: adoption.manual_only,
                snapshot_name: Some(snapshot_name),
                adopted: Some(today.to_string()),
                retired: None,
            });
        }
    }
    out
}

fn entry_mut<'a>(config: &'a mut Config, reference: &EntryRef) -> Option<&'a mut SubvolConfig> {
    config
        .sources
        .iter_mut()
        .find(|s| s.label == reference.source_label)?
        .subvolumes
        .iter_mut()
        .find(|e| e.name == reference.name)
}
```

- [ ] **Step 4: Run** `cd indexer && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` → PASS. If a test fails, fix the code, not the expectation; each expectation is a decision recorded in the spec.

- [ ] **Step 5: Mutation gate, then commit**

```bash
git add indexer/src/adopt.rs
git commit -m "adopt: plan and apply — adopt what exists, retire what is gone

Pure functions over the config and per-volume listings. A nested
subvolume joins its nearest live ancestor's source; one with no
ancestor joins a primary-only adoption source. A volume that could not
be listed changes nothing on that volume.

bd: DAS-Backup-Manager-gte"
```

---

### Task 7: The sync shell — list, write, report

**Files:**
- Modify: `indexer/src/adopt.rs`

**Interfaces:**
- Consumes: `fsutil::{CommandRunner, write_atomic}`, `health::is_mountpoint`, `caldate::today`, `btrbk_conf::render_btrbk_conf`, `Config::{load, save, validate}`, `plan_sync`, `apply_plan`
- Produces:

```rust
pub struct SyncOutcome { pub plan: SyncPlan, pub written: bool, pub write_error: Option<String> }
impl SyncOutcome { pub fn failed(&self) -> bool }
pub fn list_volume(runner: &dyn CommandRunner, is_mountpoint: &dyn Fn(&Path) -> bool, volume: &str, device: &str) -> VolumeListing
pub fn sync_subvolumes(config_path: &Path, dry_run: bool, today: &str, runner: &dyn CommandRunner, is_mountpoint: &dyn Fn(&Path) -> bool) -> Result<SyncOutcome, String>
pub fn format_sync_report(outcome: &SyncOutcome, dry_run: bool) -> String
```

`sync_subvolumes` returns `Err` only when the config cannot be loaded at all.

- [ ] **Step 1: Write the failing tests** — add to `adopt.rs` tests a scripted runner and these cases:

```rust
    use crate::fsutil::CommandRunner;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::{Command, ExitStatus, Output};
    use std::sync::Mutex;

    /// Answers by the command's full argv, joined with spaces. An unscripted
    /// command fails, so a test cannot pass by accident on a call it never
    /// expected.
    struct Scripted {
        answers: Vec<(String, i32, String)>,
        calls: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(answers: &[(&str, i32, &str)]) -> Self {
            Self {
                answers: answers.iter().map(|(k, c, o)| (k.to_string(), *c, o.to_string())).collect(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for Scripted {
        fn output(&self, cmd: &mut Command) -> std::io::Result<Output> {
            let argv = std::iter::once(cmd.get_program())
                .chain(cmd.get_args())
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.lock().unwrap().push(argv.clone());
            let (code, stdout) = self
                .answers
                .iter()
                .find(|(k, _, _)| *k == argv)
                .map(|(_, c, o)| (*c, o.clone()))
                .unwrap_or((1, String::new()));
            Ok(Output {
                status: ExitStatus::from_raw(code << 8),
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            })
        }
    }

    const MOUNTED: &dyn Fn(&Path) -> bool = &|_| true;
    const UNMOUNTED: &dyn Fn(&Path) -> bool = &|_| false;

    fn healthy(volume: &str, uuid: &str, paths: &[&str]) -> Vec<(String, i32, String)> {
        let list: String = paths.iter().map(|p| format!("ID 1 gen 1 top level 5 path {p}\n")).collect();
        vec![
            (format!("findmnt -n -o UUID,FSROOT --target {volume}"), 0, format!("{uuid} /\n")),
            (format!("btrfs subvolume list {volume}"), 0, list),
        ]
    }

    fn scripted(parts: Vec<Vec<(String, i32, String)>>) -> Scripted {
        let flat: Vec<(String, i32, String)> = parts.into_iter().flatten().collect();
        Scripted { answers: flat, calls: Mutex::new(Vec::new()) }
    }

    #[test]
    fn list_volume_reads_a_mounted_top_level_volume_with_the_expected_uuid() {
        let r = scripted(vec![healthy("/ssd", "abc", &["@", "a b"])]);
        let l = list_volume(&r, MOUNTED, "/ssd", "UUID=abc");
        assert_eq!(l.subvolumes.unwrap(), ["@", "a b"]);
    }

    #[test]
    fn list_volume_refuses_an_unmounted_path_without_running_anything() {
        let r = Scripted::new(&[]);
        let l = list_volume(&r, UNMOUNTED, "/ssd", "UUID=abc");
        assert!(l.subvolumes.unwrap_err().contains("not mounted"));
        assert!(r.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn wrong_uuid_volume_is_not_listed() {
        let r = scripted(vec![healthy("/ssd", "OTHER", &["@"])]);
        let l = list_volume(&r, MOUNTED, "/ssd", "UUID=abc");
        let why = l.subvolumes.unwrap_err();
        assert!(why.contains("OTHER") && why.contains("abc"), "{why}");
        assert!(!r.calls.lock().unwrap().iter().any(|c| c.starts_with("btrfs")));
    }

    #[test]
    fn list_volume_refuses_a_volume_not_mounted_at_its_top_level() {
        let r = Scripted::new(&[("findmnt -n -o UUID,FSROOT --target /ssd", 0, "abc /@\n")]);
        let why = list_volume(&r, MOUNTED, "/ssd", "UUID=abc").subvolumes.unwrap_err();
        assert!(why.contains("top level"), "{why}");
    }

    #[test]
    fn list_volume_resolves_a_device_path_through_blkid() {
        let mut parts = healthy("/nvme", "n1", &["@"]);
        parts.push(("blkid -s UUID -o value /dev/nvme1n1p2".into(), 0, "n1\n".into()));
        let l = list_volume(&scripted(vec![parts]), MOUNTED, "/nvme", "/dev/nvme1n1p2");
        assert_eq!(l.subvolumes.unwrap(), ["@"]);
        // blkid failing means the expected UUID is unknown: refuse.
        let l = list_volume(&scripted(vec![healthy("/nvme", "n1", &["@"])]), MOUNTED, "/nvme", "/dev/nvme1n1p2");
        assert!(l.subvolumes.unwrap_err().contains("blkid"));
    }

    #[test]
    fn list_volume_reports_a_failed_or_empty_btrfs_listing() {
        let r = Scripted::new(&[
            ("findmnt -n -o UUID,FSROOT --target /ssd", 0, "abc /\n"),
            ("btrfs subvolume list /ssd", 1, ""),
        ]);
        assert!(list_volume(&r, MOUNTED, "/ssd", "UUID=abc").subvolumes.unwrap_err().contains("btrfs subvolume list"));
        let r = scripted(vec![healthy("/ssd", "abc", &[])]);
        assert!(list_volume(&r, MOUNTED, "/ssd", "UUID=abc").subvolumes.unwrap_err().contains("no subvolumes"));
    }

    /// A config on disk with one source, plus the path its btrbk.conf goes to.
    fn on_disk_config(dir: &Path) -> std::path::PathBuf {
        let mut c = config();
        c.sources.truncate(1); // "ssd" on /ssd: @srv, @opt
        c.sources[0].device = "UUID=abc".into();
        c.general.btrbk_conf = dir.join("btrbk.conf").to_string_lossy().into_owned();
        let path = dir.join("config.toml");
        c.save(&path).unwrap();
        std::fs::write(dir.join("btrbk.conf"), "OLD").unwrap();
        path
    }

    #[test]
    fn sync_writes_config_and_btrbk_conf_when_something_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt", "@srv/web"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, MOUNTED).unwrap();
        assert!(out.written && !out.failed(), "{:?}", out.write_error);
        let saved = Config::load(&path).unwrap();
        assert!(saved.sources[0].subvolumes.iter().any(|s| s.name == "@srv/web"));
        let conf = std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap();
        assert!(conf.contains("subvolume             @srv/web\n    snapshot_name       srv-web\n"), "{conf}");
    }

    #[test]
    fn sync_touches_nothing_when_nothing_changed_or_on_a_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();

        let same = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &same, MOUNTED).unwrap();
        assert!(!out.written);
        assert_eq!(std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(), "OLD");

        let more = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt", "@srv/web"])]);
        let out = sync_subvolumes(&path, true, "2026-10-02", &more, MOUNTED).unwrap();
        assert!(!out.written);
        assert_eq!(out.plan.adopt.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(), "OLD");
    }

    #[test]
    fn sync_keeps_both_files_when_the_new_config_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();
        // Occupy the temp-file name so the atomic write fails.
        std::fs::create_dir(dir.path().join(".btrbk.conf.tmp")).unwrap();
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt", "@srv/web"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, MOUNTED).unwrap();
        assert!(!out.written);
        assert!(out.failed());
        assert!(out.write_error.as_deref().unwrap().contains("btrbk.conf"), "{:?}", out.write_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(), "OLD");
    }

    #[test]
    fn sync_fails_when_a_volume_cannot_be_read_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let out = sync_subvolumes(&path, false, "2026-10-02", &Scripted::new(&[]), UNMOUNTED).unwrap();
        assert!(out.failed() && !out.written);
        assert_eq!(out.plan.failed_volumes.len(), 1);
    }

    #[test]
    fn sync_errors_only_when_the_config_cannot_be_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let err = sync_subvolumes(&dir.path().join("absent.toml"), false, "2026-10-02", &Scripted::new(&[]), MOUNTED)
            .unwrap_err();
        assert!(err.contains("absent.toml"), "{err}");
    }

    #[test]
    fn report_lists_every_decision_and_says_when_nothing_changed() {
        let quiet = SyncOutcome { plan: SyncPlan::default(), written: false, write_error: None };
        assert_eq!(
            format_sync_report(&quiet, false),
            "SUBVOLUME SYNC\n  No new, vanished or returning subvolumes.\n"
        );

        let plan = SyncPlan {
            adopt: vec![
                Adoption { volume: "/ssd".into(), name: "@srv/web".into(), source_label: "ssd".into(), nested_under: Some("@srv".into()), manual_only: false },
                Adoption { volume: "/ssd".into(), name: "@new".into(), source_label: "ssd-adopted".into(), nested_under: None, manual_only: false },
            ],
            retire: vec![EntryRef { source_label: "ssd".into(), name: "@opt".into() }],
            revive: vec![EntryRef { source_label: "ssd".into(), name: "@old".into() }],
            skipped: vec![Skip { volume: "/ssd".into(), name: "@cache/x".into(), reason: SkipReason::Excluded { pattern: "@cache".into() } }],
            unplaceable: vec![Unplaceable { volume: "/hdd".into(), name: "top".into(), why: "no primary".into() }],
            failed_volumes: vec![("/nvme".into(), "not mounted".into())],
        };
        let text = format_sync_report(&SyncOutcome { plan, written: true, write_error: None }, false);
        assert_eq!(
            text,
            "SUBVOLUME SYNC\n\
             \x20 Adopted (now backed up):\n\
             \x20   @srv/web  [/ssd -> source ssd, as its parent @srv]\n\
             \x20   @new  [/ssd -> source ssd-adopted, primary target only]\n\
             \x20 Retired (gone from disk; existing backups will expire):\n\
             \x20   @opt  [source ssd]\n\
             \x20 Revived (back on disk):\n\
             \x20   @old  [source ssd]\n\
             \x20 Skipped:\n\
             \x20   @cache/x  [/ssd, excluded by '@cache']\n\
             \x20 COULD NOT BE PLACED (not backed up):\n\
             \x20   top  [/hdd: no primary]\n\
             \x20 VOLUMES NOT READ (nothing adopted or retired there):\n\
             \x20   /nvme: not mounted\n"
        );
    }

    #[test]
    fn report_marks_a_dry_run_and_a_failed_write() {
        let plan = SyncPlan {
            adopt: vec![Adoption { volume: "/v".into(), name: "a".into(), source_label: "s".into(), nested_under: None, manual_only: false }],
            ..Default::default()
        };
        let dry = format_sync_report(&SyncOutcome { plan: plan.clone(), written: false, write_error: None }, true);
        assert!(dry.contains("  Would adopt (dry run, nothing written):\n"), "{dry}");
        let bad = format_sync_report(
            &SyncOutcome { plan, written: false, write_error: Some("disk full".into()) },
            false,
        );
        assert!(bad.contains("  CONFIG NOT UPDATED: disk full\n"), "{bad}");
        assert!(bad.contains("  NOT adopted (config could not be written):\n"), "{bad}");
    }
```

- [ ] **Step 2: Run and watch them fail** — `cd indexer && cargo test --lib adopt` → FAIL to compile.

- [ ] **Step 3: Implement**

```rust
use std::path::Path;
use std::process::Command;

use crate::fsutil::{CommandRunner, write_atomic};

fn run_stdout(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> Result<String, String> {
    let what = format!("{program} {}", args.join(" "));
    match runner.output(Command::new(program).args(args)) {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => Err(format!("'{what}' failed: {}", String::from_utf8_lossy(&out.stderr).trim())),
        Err(e) => Err(format!("'{what}' could not be run: {e}")),
    }
}

/// List the subvolumes on one source volume, after proving the path is a
/// real mountpoint holding the expected filesystem at its top level.
///
/// `btrfs subvolume list` on an unmounted directory answers for whatever
/// filesystem the directory sits on, and succeeds. Believing that answer
/// would retire every entry on the volume.
pub fn list_volume(
    runner: &dyn CommandRunner,
    is_mountpoint: &dyn Fn(&Path) -> bool,
    volume: &str,
    device: &str,
) -> VolumeListing {
    let listed = (|| {
        if !is_mountpoint(Path::new(volume)) {
            return Err(format!("{volume} is not mounted"));
        }
        let expected = match device.strip_prefix("UUID=") {
            Some(uuid) => uuid.to_string(),
            None => {
                let uuid = run_stdout(runner, "blkid", &["-s", "UUID", "-o", "value", device])?;
                let uuid = uuid.trim().to_string();
                if uuid.is_empty() {
                    return Err(format!("blkid reported no UUID for {device}"));
                }
                uuid
            }
        };
        let found = run_stdout(runner, "findmnt", &["-n", "-o", "UUID,FSROOT", "--target", volume])?;
        let mut fields = found.split_whitespace();
        let (uuid, fsroot) = (fields.next().unwrap_or(""), fields.next().unwrap_or(""));
        if uuid != expected {
            return Err(format!(
                "{volume} holds filesystem '{uuid}', expected '{expected}'"
            ));
        }
        if fsroot != "/" {
            return Err(format!(
                "{volume} is mounted at subvolume '{fsroot}', not at the filesystem's top level"
            ));
        }
        let out = run_stdout(runner, "btrfs", &["subvolume", "list", volume])?;
        Ok(parse_subvolume_paths(&out))
    })();
    normalize_listing(volume, listed)
}

/// What a sync did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    pub plan: SyncPlan,
    /// Whether `config.toml` and `btrbk.conf` were replaced.
    pub written: bool,
    /// Why they were not, when the plan called for it.
    pub write_error: Option<String>,
}

impl SyncOutcome {
    pub fn failed(&self) -> bool {
        self.plan.failed() || self.write_error.is_some()
    }
}

/// Bring `config.toml` and `btrbk.conf` into line with the subvolumes on the
/// already-mounted source volumes. Mounts and unmounts nothing.
pub fn sync_subvolumes(
    config_path: &Path,
    dry_run: bool,
    today: &str,
    runner: &dyn CommandRunner,
    is_mountpoint: &dyn Fn(&Path) -> bool,
) -> Result<SyncOutcome, String> {
    let config = Config::load(config_path)
        .map_err(|e| format!("could not load {}: {e}", config_path.display()))?;

    let mut listings: Vec<VolumeListing> = Vec::new();
    for source in &config.sources {
        if !listings.iter().any(|l| l.volume == source.volume) {
            listings.push(list_volume(runner, is_mountpoint, &source.volume, &source.device));
        }
    }

    let plan = plan_sync(&config, &listings);
    if dry_run || !plan.changes_config() {
        return Ok(SyncOutcome { plan, written: false, write_error: None });
    }

    let updated = apply_plan(&config, &plan, today);
    let write_error = (|| {
        let errors = updated.validate();
        if !errors.is_empty() {
            return Some(format!("the updated config is not valid: {}", errors.join("; ")));
        }
        // btrbk.conf first: if it cannot be written, config.toml is left
        // describing what btrbk will actually do.
        let conf_path = Path::new(&updated.general.btrbk_conf);
        if let Err(e) = write_atomic(conf_path, &crate::btrbk_conf::render_btrbk_conf(&updated)) {
            return Some(format!("could not write {}: {e}", conf_path.display()));
        }
        if let Err(e) = updated.save(config_path) {
            // Put btrbk.conf back so the two files still agree.
            let restore = write_atomic(conf_path, &crate::btrbk_conf::render_btrbk_conf(&config));
            return Some(match restore {
                Ok(()) => format!("could not write {}: {e}", config_path.display()),
                Err(r) => format!(
                    "could not write {}: {e}; and {} could not be restored: {r}",
                    config_path.display(),
                    conf_path.display()
                ),
            });
        }
        None
    })();

    Ok(SyncOutcome {
        written: write_error.is_none(),
        plan,
        write_error,
    })
}

/// The "SUBVOLUME SYNC" section of the run report.
pub fn format_sync_report(outcome: &SyncOutcome, dry_run: bool) -> String {
    let plan = &outcome.plan;
    let mut r = String::from("SUBVOLUME SYNC\n");
    if !plan.changes_config() && !plan.failed() && plan.skipped.is_empty() {
        r.push_str("  No new, vanished or returning subvolumes.\n");
        return r;
    }
    if let Some(why) = &outcome.write_error {
        r.push_str(&format!("  CONFIG NOT UPDATED: {why}\n"));
    }
    let applied = outcome.written;
    let heading = |done: &str, would: &str, not: &str| -> String {
        if dry_run {
            format!("  {would} (dry run, nothing written):\n")
        } else if applied {
            format!("  {done}:\n")
        } else {
            format!("  {not} (config could not be written):\n")
        }
    };
    if !plan.adopt.is_empty() {
        r.push_str(&heading("Adopted (now backed up)", "Would adopt", "NOT adopted"));
        for a in &plan.adopt {
            let how = match &a.nested_under {
                Some(parent) => format!("as its parent {parent}"),
                None => "primary target only".to_string(),
            };
            r.push_str(&format!(
                "    {}  [{} -> source {}, {how}]\n",
                a.name, a.volume, a.source_label
            ));
        }
    }
    if !plan.retire.is_empty() {
        r.push_str(&heading(
            "Retired (gone from disk; existing backups will expire)",
            "Would retire",
            "NOT retired",
        ));
        for e in &plan.retire {
            r.push_str(&format!("    {}  [source {}]\n", e.name, e.source_label));
        }
    }
    if !plan.revive.is_empty() {
        r.push_str(&heading("Revived (back on disk)", "Would revive", "NOT revived"));
        for e in &plan.revive {
            r.push_str(&format!("    {}  [source {}]\n", e.name, e.source_label));
        }
    }
    if !plan.skipped.is_empty() {
        r.push_str("  Skipped:\n");
        for s in &plan.skipped {
            let why = match &s.reason {
                SkipReason::SnapshotTree => "snapshot tree".to_string(),
                SkipReason::Excluded { pattern } => format!("excluded by '{pattern}'"),
            };
            r.push_str(&format!("    {}  [{}, {why}]\n", s.name, s.volume));
        }
    }
    if !plan.unplaceable.is_empty() {
        r.push_str("  COULD NOT BE PLACED (not backed up):\n");
        for u in &plan.unplaceable {
            r.push_str(&format!("    {}  [{}: {}]\n", u.name, u.volume, u.why));
        }
    }
    if !plan.failed_volumes.is_empty() {
        r.push_str("  VOLUMES NOT READ (nothing adopted or retired there):\n");
        for (volume, why) in &plan.failed_volumes {
            r.push_str(&format!("    {volume}: {why}\n"));
        }
    }
    r
}
```

Note the heading test in Step 1 expects a quiet report only when there is truly nothing to say; a run whose only content is skipped subvolumes lists them (an exclusion is never invisible).

- [ ] **Step 4: Run** `cd indexer && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` → PASS.

- [ ] **Step 5: Mutation gate, then commit**

```bash
git add indexer/src/adopt.rs
git commit -m "adopt: sync shell — verified listing, atomic write of both files, report

A volume is listed only if it is a real mountpoint holding the expected
filesystem at its top level. config.toml and btrbk.conf are replaced
together or not at all.

bd: DAS-Backup-Manager-gte"
```

---

### Task 8: Retention window and series matching (pure half of expiry)

**Files:**
- Create: `indexer/src/expire.rs`
- Modify: `indexer/src/lib.rs` (`pub mod expire;`)

**Interfaces:**
- Consumes: `config::Retention`, `caldate::day_number`
- Produces:
  - `pub fn expire::longest_window_days(r: &Retention) -> Option<u32>`
  - `pub fn expire::is_expired(retired: &str, window_days: u32, today: &str) -> Option<bool>`
  - `pub fn expire::expiry_date(retired: &str, window_days: u32) -> Option<String>`
  - `pub fn expire::series_snapshots(entries: &[String], snapshot_name: &str) -> Vec<String>`

- [ ] **Step 1: Write the failing tests** (`indexer/src/expire.rs`)

```rust
//! Expiry of the backups a retired subvolume left behind.
//!
//! Rule (spec §9): every snapshot of a retired series stays on a target until
//! the retirement date plus that target's longest retention window, then all
//! of them are deleted from that target together. btrbk cannot do this:
//! `btrbk prune` skips deletion when the source is not accessible.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Retention;

    fn r(daily: u32, weekly: u32, monthly: u32, yearly: u32) -> Retention {
        Retention { daily, weekly, monthly, yearly }
    }

    #[test]
    fn longest_window_is_the_longest_tier_in_whole_days() {
        assert_eq!(longest_window_days(&r(7, 0, 0, 0)), Some(7));
        assert_eq!(longest_window_days(&r(7, 4, 0, 0)), Some(28));
        assert_eq!(longest_window_days(&r(7, 4, 12, 0)), Some(372));
        assert_eq!(longest_window_days(&r(7, 4, 12, 1)), Some(372));
        assert_eq!(longest_window_days(&r(0, 0, 0, 1)), Some(366));
        assert_eq!(longest_window_days(&r(30, 1, 0, 0)), Some(30));
        // No retention configured is not "expire at once": there is no window.
        assert_eq!(longest_window_days(&r(0, 0, 0, 0)), None);
    }

    #[test]
    fn a_series_expires_only_after_the_whole_window_has_passed() {
        // Retired on the 1st with a 7-day window: kept through the 8th.
        assert_eq!(is_expired("2026-10-01", 7, "2026-10-01"), Some(false));
        assert_eq!(is_expired("2026-10-01", 7, "2026-10-07"), Some(false));
        assert_eq!(is_expired("2026-10-01", 7, "2026-10-08"), Some(false));
        assert_eq!(is_expired("2026-10-01", 7, "2026-10-09"), Some(true));
        // A clock set back before the retirement date expires nothing.
        assert_eq!(is_expired("2026-10-01", 7, "2026-09-01"), Some(false));
        assert_eq!(expiry_date("2026-10-01", 7).as_deref(), Some("2026-10-09"));
    }

    #[test]
    fn an_unreadable_date_never_expires_anything() {
        assert_eq!(is_expired("not-a-date", 7, "2026-10-09"), None);
        assert_eq!(is_expired("2026-10-01", 7, "garbage"), None);
        assert_eq!(expiry_date("", 7), None);
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn series_snapshots_does_not_match_a_longer_name() {
        let dir = names(&[
            "home.20261001T1421",
            "home.20260930T0323",
            "home-video.20261001T1421",
            "home.20261001T1421_1",
            "home.20261001",
            "home.20261001T142100",
            "home",
            "home.tmp",
            "home.2026",
            "home.20261001T1421.partial",
            "xhome.20261001T1421",
        ]);
        assert_eq!(
            series_snapshots(&dir, "home"),
            [
                "home.20260930T0323",
                "home.20261001",
                "home.20261001T1421",
                "home.20261001T142100",
                "home.20261001T1421_1",
            ]
        );
        assert_eq!(series_snapshots(&dir, "home-video"), ["home-video.20261001T1421"]);
        assert!(series_snapshots(&dir, "").is_empty());
    }

    #[test]
    fn series_snapshots_treats_the_name_literally() {
        // '.' and '*' in a snapshot name are characters, not patterns.
        let dir = names(&["a.b.20261001T1421", "aXb.20261001T1421", "a*.20261001T1421"]);
        assert_eq!(series_snapshots(&dir, "a.b"), ["a.b.20261001T1421"]);
        assert_eq!(series_snapshots(&dir, "a*"), ["a*.20261001T1421"]);
    }
}
```

- [ ] **Step 2: Run and watch them fail** — `cd indexer && cargo test --lib expire` → FAIL to compile.

- [ ] **Step 3: Implement** (above the tests)

```rust
use crate::caldate::{date_of, day_number};
use crate::config::Retention;

/// The longest of a target's retention tiers, in whole days: a daily tier
/// counts 1 day each, weekly 7, monthly 31, yearly 366. Rounded up on
/// purpose — the cost of rounding is keeping a backup a little longer.
/// `None` when no tier is set: there is then no window to measure against,
/// and nothing may be expired on that target.
pub fn longest_window_days(r: &Retention) -> Option<u32> {
    let longest = [
        r.daily,
        r.weekly.saturating_mul(7),
        r.monthly.saturating_mul(31),
        r.yearly.saturating_mul(366),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    (longest > 0).then_some(longest)
}

/// The first day on which a series retired on `retired` may be deleted.
pub fn expiry_date(retired: &str, window_days: u32) -> Option<String> {
    Some(date_of(day_number(retired)? + i64::from(window_days) + 1))
}

/// Whether a series retired on `retired` is past its window on `today`.
/// `None` if either date cannot be read — and an unreadable date must never
/// be the reason a backup is deleted.
pub fn is_expired(retired: &str, window_days: u32, today: &str) -> Option<bool> {
    Some(day_number(today)? > day_number(retired)? + i64::from(window_days))
}

fn is_btrbk_timestamp(s: &str) -> bool {
    // btrbk: YYYYMMDD, optionally Thhmm or Thhmmss, optionally _N.
    let (stamp, counter) = match s.split_once('_') {
        Some((stamp, n)) => (stamp, Some(n)),
        None => (s, None),
    };
    if counter.is_some_and(|n| n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let all_digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    match stamp.split_once('T') {
        Some((date, time)) => {
            date.len() == 8 && all_digits(date) && (time.len() == 4 || time.len() == 6) && all_digits(time)
        }
        None => stamp.len() == 8 && all_digits(stamp),
    }
}

/// The entries of a directory that are snapshots of exactly this series:
/// `<snapshot_name>.<btrbk timestamp>`. Sorted. A longer name that merely
/// starts the same (`home-video` for `home`) is a different series.
pub fn series_snapshots(entries: &[String], snapshot_name: &str) -> Vec<String> {
    if snapshot_name.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<String> = entries
        .iter()
        .filter(|entry| {
            entry
                .strip_prefix(snapshot_name)
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(is_btrbk_timestamp)
        })
        .cloned()
        .collect();
    found.sort();
    found
}
```

- [ ] **Step 4: Run** `cd indexer && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test --lib expire` → PASS.

- [ ] **Step 5: Mutation gate, then commit**

```bash
git add indexer/src/expire.rs indexer/src/lib.rs
git commit -m "expire: retention window in days, and exact series matching

bd: DAS-Backup-Manager-gte"
```

---

### Task 9: Expiry shell — delete what is past its window

**Files:**
- Modify: `indexer/src/expire.rs`

**Interfaces:**
- Consumes: Task 8's functions; `Config`, `Source`, `Target`, `TargetRole`; `btrbk_conf::resolve_snapshot_names`; `fsutil::CommandRunner`; `Config::save`
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationState {
    /// The directory could not be examined (target not attached, not a mountpoint, unreadable).
    Unreachable(String),
    /// Snapshots remain; `expires` is None when the target has no retention window.
    Kept { count: usize, expires: Option<String> },
    Deleted { count: usize },
    DeleteFailed { deleted: usize, errors: Vec<String> },
    Empty,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationReport { pub place: String, pub state: LocationState, pub deleted_paths: Vec<PathBuf> }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredReport { pub source_label: String, pub name: String, pub snapshot_name: String, pub retired: String, pub locations: Vec<LocationReport>, pub removed_from_config: bool }
pub struct ExpireOutcome { pub entries: Vec<RetiredReport>, pub config_error: Option<String> }
impl ExpireOutcome { pub fn failed(&self) -> bool; pub fn deleted_paths(&self) -> Vec<PathBuf> }
pub fn expire_retired(config_path: &Path, dry_run: bool, today: &str, runner: &dyn CommandRunner, is_mountpoint: &dyn Fn(&Path) -> bool) -> Result<ExpireOutcome, String>
pub fn format_expire_report(outcome: &ExpireOutcome, dry_run: bool) -> String
```

Locations examined for one retired entry, in order: one per target the entry's source sends to (`<target.mount>/<source.target_subdirs.first() or source.label>`, window = that target's `longest_window_days`), then the source snapshot directory (`<source.volume>/<source.snapshot_dir>`, window = the **shortest** window among those targets, `None` if none has one). A target is "sent to" when its role is primary or mirror and `source.target_labels` is empty or contains its label — the same filter `render_btrbk_conf` uses. A location is reachable only if its mount root (`target.mount`, or `source.volume`) satisfies `is_mountpoint`.

Deletion is `btrfs subvolume delete <path>` through the runner. The entry is removed from the config only when every location is `Empty` or `Deleted` (so: never while any location is `Unreachable`, `Kept` or `DeleteFailed`), and never on a dry run.

- [ ] **Step 1: Write the failing tests** — add to `expire.rs` tests. Reuse the `Scripted` runner by moving it from `adopt.rs` tests into a `#[cfg(test)] pub(crate) mod testing` in `fsutil.rs` and importing it in both test modules (`use crate::fsutil::testing::Scripted;`); add to it a helper `fn calls(&self) -> Vec<String>`.

```rust
    use crate::config::{Config, Source, SubvolConfig, Target, TargetRole};
    use crate::fsutil::testing::Scripted;
    use std::path::{Path, PathBuf};

    struct Rig {
        dir: tempfile::TempDir,
        config_path: PathBuf,
    }

    /// One source on `<dir>/vol` sending to a primary (`<dir>/big`, yearly 1)
    /// and a mirror (`<dir>/small`, daily 7). `@opt` is retired on 2026-10-01
    /// with snapshot name `opt`; `@srv` is live.
    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let p = |s: &str| dir.path().join(s).to_string_lossy().into_owned();
        for d in ["vol/.btrbk-snapshots", "big/ssd", "small/ssd"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        let mut c = Config::default();
        let target = |label: &str, role, retention| Target {
            label: label.into(),
            serial: "S".into(),
            serials: vec!["S".into()],
            mount_uuid: None,
            mount: p(label),
            role,
            retention,
            display_name: String::new(),
        };
        c.targets = vec![
            target("big", TargetRole::Primary, r(7, 4, 12, 1)),
            target("small", TargetRole::Mirror, r(7, 0, 0, 0)),
        ];
        c.sources = vec![Source {
            label: "ssd".into(),
            volume: p("vol"),
            subvolumes: vec![
                SubvolConfig { name: "@srv".into(), ..Default::default() },
                SubvolConfig { name: "@opt".into(), retired: Some("2026-10-01".into()), ..Default::default() },
            ],
            device: "UUID=abc".into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec!["ssd".into()],
            target_labels: Vec::new(),
        }];
        c.general.btrbk_conf = p("btrbk.conf");
        let config_path = dir.path().join("config.toml");
        c.save(&config_path).unwrap();
        Rig { dir, config_path }
    }

    impl Rig {
        fn snap(&self, place: &str, name: &str) -> PathBuf {
            let path = self.dir.path().join(place).join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
        /// A runner whose `btrfs subvolume delete <path>` really removes the
        /// directory, so "what remains" can be asserted afterwards.
        fn deleting(&self, paths: &[&PathBuf]) -> Scripted {
            Scripted::deleting(paths.iter().map(|p| p.to_string_lossy().into_owned()).collect())
        }
    }

    const MOUNTED: &dyn Fn(&Path) -> bool = &|_| true;

    #[test]
    fn nothing_is_deleted_inside_the_window() {
        let rig = rig();
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let big = rig.snap("big/ssd", "opt.20260930T0323");
        let runner = Scripted::new(&[]);
        let out = expire_retired(&rig.config_path, false, "2026-10-08", &runner, MOUNTED).unwrap();
        assert!(small.exists() && big.exists());
        assert!(runner.calls().is_empty());
        assert!(!out.failed());
        let entry = &out.entries[0];
        assert_eq!((entry.name.as_str(), entry.snapshot_name.as_str()), ("@opt", "opt"));
        assert_eq!(
            entry.locations.iter().map(|l| l.state.clone()).collect::<Vec<_>>(),
            vec![
                LocationState::Kept { count: 1, expires: Some("2027-10-09".into()) },
                LocationState::Kept { count: 1, expires: Some("2026-10-09".into()) },
                LocationState::Empty,
            ]
        );
        assert!(!entry.removed_from_config);
    }

    #[test]
    fn each_target_expires_on_its_own_window_and_only_the_retired_series() {
        let rig = rig();
        let small_a = rig.snap("small/ssd", "opt.20260929T0323");
        let small_b = rig.snap("small/ssd", "opt.20260930T0323");
        let small_live = rig.snap("small/ssd", "srv.20260930T0323");
        let small_other = rig.snap("small/ssd", "opt-extra.20260930T0323");
        let big = rig.snap("big/ssd", "opt.20260930T0323");
        let src = rig.snap("vol/.btrbk-snapshots", "opt.20260930T0323");

        let runner = rig.deleting(&[&small_a, &small_b, &src]);
        let out = expire_retired(&rig.config_path, false, "2026-10-09", &runner, MOUNTED).unwrap();

        assert!(!small_a.exists() && !small_b.exists(), "past the 7-day window");
        assert!(!src.exists(), "source snapshots follow the shortest target window");
        assert!(big.exists(), "the primary keeps it for its own, longer window");
        assert!(small_live.exists() && small_other.exists(), "other series are never touched");
        assert_eq!(
            runner.calls(),
            [&small_a, &small_b, &src]
                .iter()
                .map(|p| format!("btrfs subvolume delete {}", p.display()))
                .collect::<Vec<_>>()
        );
        assert_eq!(out.deleted_paths(), vec![small_a, small_b, src]);
        // Still on the primary, so the entry stays.
        assert!(!out.entries[0].removed_from_config);
        assert!(Config::load(&rig.config_path).unwrap().sources[0].subvolumes.iter().any(|s| s.name == "@opt"));
    }

    #[test]
    fn entry_is_removed_once_no_snapshot_remains_anywhere() {
        let rig = rig();
        let big = rig.snap("big/ssd", "opt.20260930T0323");
        let runner = rig.deleting(&[&big]);
        let out = expire_retired(&rig.config_path, false, "2028-01-01", &runner, MOUNTED).unwrap();
        assert!(out.entries[0].removed_from_config);
        let saved = Config::load(&rig.config_path).unwrap();
        assert_eq!(
            saved.sources[0].subvolumes.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["@srv"]
        );
    }

    #[test]
    fn entry_is_kept_while_any_location_is_unreachable() {
        let rig = rig();
        let big_mount = rig.dir.path().join("big");
        let only_big_missing: &dyn Fn(&Path) -> bool = &|p| p != big_mount;
        let out = expire_retired(&rig.config_path, false, "2028-01-01", &Scripted::new(&[]), only_big_missing).unwrap();
        let states: Vec<_> = out.entries[0].locations.iter().map(|l| l.state.clone()).collect();
        assert!(matches!(states[0], LocationState::Unreachable(_)), "{states:?}");
        assert_eq!(states[1], LocationState::Empty);
        assert!(!out.entries[0].removed_from_config);
        assert!(!out.failed(), "an absent target is a valid state, not a failure");
        assert!(Config::load(&rig.config_path).unwrap().sources[0].subvolumes.iter().any(|s| s.name == "@opt"));
    }

    #[test]
    fn a_target_with_no_retention_keeps_and_reports() {
        let rig = rig();
        let mut c = Config::load(&rig.config_path).unwrap();
        c.targets[1].retention = r(0, 0, 0, 0);
        c.save(&rig.config_path).unwrap();
        let small = rig.snap("small/ssd", "opt.20200101T0000");
        let runner = Scripted::new(&[]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, MOUNTED).unwrap();
        assert!(small.exists());
        assert_eq!(out.entries[0].locations[1].state, LocationState::Kept { count: 1, expires: None });
        assert!(!out.entries[0].removed_from_config);
    }

    #[test]
    fn dry_run_deletes_nothing_and_keeps_the_entry() {
        let rig = rig();
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let runner = Scripted::new(&[]);
        let out = expire_retired(&rig.config_path, true, "2026-10-09", &runner, MOUNTED).unwrap();
        assert!(small.exists());
        assert!(runner.calls().is_empty());
        assert_eq!(out.entries[0].locations[1].state, LocationState::Deleted { count: 1 });
        assert!(out.deleted_paths().is_empty());
        let text = format_expire_report(&out, true);
        assert!(text.contains("would delete 1"), "{text}");
    }

    #[test]
    fn a_failed_delete_is_reported_and_marks_the_run_failed() {
        let rig = rig();
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        // No delete is scripted to succeed.
        let out = expire_retired(&rig.config_path, false, "2026-10-09", &Scripted::new(&[]), MOUNTED).unwrap();
        assert!(small.exists());
        assert!(out.failed());
        assert!(matches!(
            out.entries[0].locations[1].state,
            LocationState::DeleteFailed { deleted: 0, .. }
        ));
        assert!(!out.entries[0].removed_from_config);
    }

    #[test]
    fn an_unreadable_retirement_date_expires_nothing() {
        let rig = rig();
        let mut c = Config::load(&rig.config_path).unwrap();
        c.sources[0].subvolumes[1].retired = Some("sometime".into());
        c.save(&rig.config_path).unwrap();
        let small = rig.snap("small/ssd", "opt.20200101T0000");
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &Scripted::new(&[]), MOUNTED).unwrap();
        assert!(small.exists());
        assert_eq!(out.entries[0].locations[1].state, LocationState::Kept { count: 1, expires: None });
    }

    #[test]
    fn report_is_empty_when_nothing_is_retired_and_lists_every_location_otherwise() {
        let rig = rig();
        let mut c = Config::load(&rig.config_path).unwrap();
        c.sources[0].subvolumes[1].retired = None;
        c.save(&rig.config_path).unwrap();
        let out = expire_retired(&rig.config_path, false, "2026-10-08", &Scripted::new(&[]), MOUNTED).unwrap();
        assert_eq!(format_expire_report(&out, false), "");

        let rig = rig();
        rig.snap("small/ssd", "opt.20260930T0323");
        let out = expire_retired(&rig.config_path, false, "2026-10-08", &Scripted::new(&[]), MOUNTED).unwrap();
        let text = format_expire_report(&out, false);
        assert!(text.starts_with("RETIRED SUBVOLUMES\n  @opt  [source ssd, retired 2026-10-01, series 'opt']\n"), "{text}");
        assert!(text.contains("no snapshots"), "{text}");
        assert!(text.contains("1 snapshot kept until 2026-10-09"), "{text}");
    }
```

`Scripted::deleting(paths)` (in `fsutil::testing`): a runner that, for the argv `btrfs subvolume delete <path>` with `<path>` in its list, removes that directory with `std::fs::remove_dir_all` and returns exit 0; everything else behaves as `Scripted::new(&[])` (exit 1).

- [ ] **Step 2: Run and watch them fail** — `cd indexer && cargo test --lib expire` → FAIL to compile.

- [ ] **Step 3: Implement** in `expire.rs`:

```rust
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::btrbk_conf::resolve_snapshot_names;
use crate::config::{Config, Source, Target, TargetRole};
use crate::fsutil::CommandRunner;

// (LocationState, LocationReport, RetiredReport exactly as in Interfaces)

pub struct ExpireOutcome {
    pub entries: Vec<RetiredReport>,
    /// Why a fully expired entry could not be removed from the config.
    pub config_error: Option<String>,
}

impl ExpireOutcome {
    /// A delete that failed, or a config that could not be saved. An
    /// unreachable target is not a failure: targets are allowed to be absent.
    pub fn failed(&self) -> bool {
        self.config_error.is_some()
            || self.entries.iter().flat_map(|e| &e.locations).any(|l| {
                matches!(l.state, LocationState::DeleteFailed { .. })
            })
    }

    /// Every snapshot path actually deleted, for pruning the index.
    pub fn deleted_paths(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .flat_map(|e| &e.locations)
            .flat_map(|l| l.deleted_paths.iter().cloned())
            .collect()
    }
}

fn targets_of<'a>(config: &'a Config, source: &Source) -> Vec<&'a Target> {
    config
        .targets
        .iter()
        .filter(|t| t.role == TargetRole::Primary || t.role == TargetRole::Mirror)
        .filter(|t| source.target_labels.is_empty() || source.target_labels.contains(&t.label))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn examine(
    place: String,
    mount_root: &Path,
    dir: &Path,
    snapshot_name: &str,
    retired: &str,
    window: Option<u32>,
    today: &str,
    dry_run: bool,
    runner: &dyn CommandRunner,
    is_mountpoint: &dyn Fn(&Path) -> bool,
) -> LocationReport {
    let report = |state| LocationReport { place: place.clone(), state, deleted_paths: Vec::new() };
    if !is_mountpoint(mount_root) {
        return report(LocationState::Unreachable(format!(
            "{} is not mounted",
            mount_root.display()
        )));
    }
    let entries: Vec<String> = match std::fs::read_dir(dir) {
        Ok(read) => read
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        // A target that never received this source has no such directory.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return report(LocationState::Unreachable(format!(
                "{} could not be read: {e}",
                dir.display()
            )));
        }
    };
    let snapshots = series_snapshots(&entries, snapshot_name);
    if snapshots.is_empty() {
        return report(LocationState::Empty);
    }
    let due = window.and_then(|w| is_expired(retired, w, today));
    if due != Some(true) {
        return report(LocationState::Kept {
            count: snapshots.len(),
            // No date when the window or the retirement date is unknown:
            // such a series is kept until someone decides.
            expires: window.filter(|_| due.is_some()).and_then(|w| expiry_date(retired, w)),
        });
    }
    if dry_run {
        return report(LocationState::Deleted { count: snapshots.len() });
    }
    let mut deleted_paths = Vec::new();
    let mut errors = Vec::new();
    for name in &snapshots {
        let path = dir.join(name);
        match runner.output(Command::new("btrfs").args(["subvolume", "delete"]).arg(&path)) {
            Ok(out) if out.status.success() => deleted_paths.push(path),
            Ok(out) => errors.push(format!(
                "{}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => errors.push(format!("{}: could not run btrfs: {e}", path.display())),
        }
    }
    let state = if errors.is_empty() {
        LocationState::Deleted { count: deleted_paths.len() }
    } else {
        LocationState::DeleteFailed { deleted: deleted_paths.len(), errors }
    };
    LocationReport { place, state, deleted_paths }
}

/// Delete the snapshots of retired subvolumes that are past their window,
/// and drop entries that have none left. Mounts and unmounts nothing: a
/// location whose mount root is not mounted is reported and left alone.
pub fn expire_retired(
    config_path: &Path,
    dry_run: bool,
    today: &str,
    runner: &dyn CommandRunner,
    is_mountpoint: &dyn Fn(&Path) -> bool,
) -> Result<ExpireOutcome, String> {
    let config = Config::load(config_path)
        .map_err(|e| format!("could not load {}: {e}", config_path.display()))?;
    let mut entries = Vec::new();

    for source in &config.sources {
        let names = resolve_snapshot_names(&source.subvolumes);
        for (entry, snapshot_name) in source.subvolumes.iter().zip(names) {
            let Some(retired) = entry.retired.clone() else { continue };
            let targets = targets_of(&config, source);
            let subdir = source.target_subdirs.first().unwrap_or(&source.label);
            let mut locations = Vec::new();
            for target in &targets {
                let root = Path::new(&target.mount);
                locations.push(examine(
                    format!("target {}", target.label),
                    root,
                    &root.join(subdir),
                    &snapshot_name,
                    &retired,
                    longest_window_days(&target.retention),
                    today,
                    dry_run,
                    runner,
                    is_mountpoint,
                ));
            }
            // Source-side snapshots only exist to be sent. Once the shortest
            // target window has passed they have no further use.
            let shortest = targets
                .iter()
                .filter_map(|t| longest_window_days(&t.retention))
                .min();
            let root = Path::new(&source.volume);
            locations.push(examine(
                format!("source {}", source.volume),
                root,
                &root.join(&source.snapshot_dir),
                &snapshot_name,
                &retired,
                shortest,
                today,
                dry_run,
                runner,
                is_mountpoint,
            ));
            entries.push(RetiredReport {
                source_label: source.label.clone(),
                name: entry.name.clone(),
                snapshot_name,
                retired,
                locations,
                removed_from_config: false,
            });
        }
    }

    let mut config_error = None;
    if !dry_run {
        let mut updated = config.clone();
        for report in &mut entries {
            let gone_everywhere = report.locations.iter().all(|l| {
                matches!(l.state, LocationState::Empty | LocationState::Deleted { .. })
            });
            if !gone_everywhere {
                continue;
            }
            if let Some(source) = updated.sources.iter_mut().find(|s| s.label == report.source_label) {
                source.subvolumes.retain(|e| !(e.name == report.name && e.retired.is_some()));
                report.removed_from_config = true;
            }
        }
        if entries.iter().any(|e| e.removed_from_config) {
            // A source left with no entries at all would fail validation;
            // drop it with its last entry.
            updated.sources.retain(|s| !s.subvolumes.is_empty());
            if let Err(e) = updated.save(config_path) {
                config_error = Some(format!("could not write {}: {e}", config_path.display()));
                for report in &mut entries {
                    report.removed_from_config = false;
                }
            }
        }
    }
    Ok(ExpireOutcome { entries, config_error })
}

/// The "RETIRED SUBVOLUMES" section of the run report. Empty when no entry
/// is retired, so the section appears only while there is something to say.
pub fn format_expire_report(outcome: &ExpireOutcome, dry_run: bool) -> String {
    if outcome.entries.is_empty() {
        return String::new();
    }
    let plural = |n: usize| if n == 1 { "snapshot" } else { "snapshots" };
    let mut r = String::from("RETIRED SUBVOLUMES\n");
    for entry in &outcome.entries {
        r.push_str(&format!(
            "  {}  [source {}, retired {}, series '{}']\n",
            entry.name, entry.source_label, entry.retired, entry.snapshot_name
        ));
        for location in &entry.locations {
            let line = match &location.state {
                LocationState::Unreachable(why) => format!("not reachable ({why}); nothing changed"),
                LocationState::Empty => "no snapshots".to_string(),
                LocationState::Kept { count, expires: Some(date) } => {
                    format!("{count} {} kept until {date}", plural(*count))
                }
                LocationState::Kept { count, expires: None } => format!(
                    "{count} {} KEPT — no retention window to measure against",
                    plural(*count)
                ),
                LocationState::Deleted { count } if dry_run => {
                    format!("would delete {count} {} (window passed)", plural(*count))
                }
                LocationState::Deleted { count } => {
                    format!("deleted {count} {} (window passed)", plural(*count))
                }
                LocationState::DeleteFailed { deleted, errors } => format!(
                    "DELETE FAILED after {deleted}: {}",
                    errors.join("; ")
                ),
            };
            r.push_str(&format!("    {}: {line}\n", location.place));
        }
        if entry.removed_from_config {
            r.push_str("    no backups remain — entry removed from config\n");
        }
    }
    if let Some(why) = &outcome.config_error {
        r.push_str(&format!("  CONFIG NOT UPDATED: {why}\n"));
    }
    r
}
```

`btrbk.conf` needs no regeneration here: retired entries were never rendered into it.

- [ ] **Step 4: Run** `cd indexer && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` → PASS.

- [ ] **Step 5: Mutation gate, then commit**

```bash
git add indexer/src/expire.rs indexer/src/fsutil.rs indexer/src/adopt.rs
git commit -m "expire: delete retired series past their window, per target

Each target is judged on its own longest retention window. A location
that is not mounted is reported and left alone, and an entry is removed
from the config only when no snapshot of it remains anywhere reachable
and every location was reachable.

bd: DAS-Backup-Manager-gte"
```

---

### Task 10: CLI — `subvol sync`, `subvol expire`, and a `subvol add` that finishes the job

**Files:**
- Modify: `indexer/src/main.rs` — `SubvolAction` (582-633), its handlers (2005-2049), `Backup → Run` handler (1541-1623)
- Modify: `indexer/tests/setup_requires_root.rs` (no change) — new file `indexer/tests/subvol_cli.rs`

**Interfaces:**
- Consumes: `adopt::{sync_subvolumes, format_sync_report}`, `expire::{expire_retired, format_expire_report}`, `caldate::today`, `fsutil::SystemRunner`, `health::is_mountpoint`, `Database::{list_snapshots, prune_snapshots}`, `btrbk_conf::render_btrbk_conf`, `fsutil::write_atomic`
- Produces (CLI contract the shell relies on):
  - `btrdasd subvol sync [--config PATH] [--dry-run]` — prints the SUBVOLUME SYNC section to stdout. Exit **0**: ran, nothing failed (whether or not anything changed). Exit **1**: a volume could not be read, a subvolume could not be placed, or the files could not be written. Exit **2**: the config could not be loaded.
  - `btrdasd subvol expire [--config PATH] [--db PATH] [--dry-run]` — prints the RETIRED SUBVOLUMES section (nothing when no entry is retired). Exit **0** / **1** / **2** on the same pattern.
  - `btrdasd subvol add|remove|set-manual|set-auto` validate the result, save atomically with the header, and regenerate `btrbk.conf`.

- [ ] **Step 1: Write the failing test** (`indexer/tests/subvol_cli.rs`)

```rust
//! `btrdasd subvol` commands that change the config must leave `config.toml`
//! and `btrbk.conf` agreeing with each other. Before this, `subvol add` wrote
//! the config and nothing else, so the subvolume was "added" and still not
//! backed up until `setup --upgrade` was also run.

use std::path::Path;
use std::process::Command;

fn write_config(dir: &Path) -> std::path::PathBuf {
    let conf = dir.join("btrbk.conf");
    let text = format!(
        r#"[general]
version = "0.7.22"
install_prefix = "/usr"
db_path = "{db}"
btrbk_conf = "{conf}"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[[source]]
label = "s"
volume = "/vol"
device = "UUID=abc"
[[source.subvolumes]]
name = "@"
[[target]]
label = "t"
serial = "X"
mount = "/mnt/t"
role = "primary"
[target.retention]
daily = 7
[email]
enabled = false
[gui]
enabled = false
"#,
        db = dir.join("index.db").display(),
        conf = conf.display(),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn btrdasd(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_btrdasd")).args(args).output().expect("spawn btrdasd")
}

#[test]
fn subvol_add_and_remove_regenerate_btrbk_conf() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let config_s = config.to_str().unwrap();

    let out = btrdasd(&["subvol", "add", "s", "@home", "--config", config_s]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let conf = std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap();
    assert!(conf.contains("  subvolume             @home\n    snapshot_name       home\n"), "{conf}");
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(saved.starts_with("# Generated by btrdasd setup"), "{saved}");

    let out = btrdasd(&["subvol", "remove", "s", "@home", "--config", config_s]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let conf = std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap();
    assert!(!conf.contains("@home"), "{conf}");
}

#[test]
fn subvol_add_refuses_a_name_collision_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let config_s = config.to_str().unwrap();
    assert!(btrdasd(&["subvol", "add", "s", "a/b", "--config", config_s]).status.success());
    let before = std::fs::read_to_string(&config).unwrap();
    let conf_before = std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap();

    let out = btrdasd(&["subvol", "add", "s", "a-b", "--config", config_s]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("snapshot name 'a-b'"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    assert_eq!(std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(), conf_before);
}

#[test]
fn subvol_sync_exits_one_when_a_volume_cannot_be_read_and_two_without_a_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    // /vol is not a mountpoint on any test machine.
    let out = btrdasd(&["subvol", "sync", "--config", config.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("SUBVOLUME SYNC\n"), "{text}");
    assert!(text.contains("VOLUMES NOT READ"), "{text}");
    assert!(text.contains("/vol: /vol is not mounted"), "{text}");

    let out = btrdasd(&["subvol", "sync", "--config", dir.path().join("none.toml").to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn subvol_expire_prints_nothing_and_exits_zero_when_nothing_is_retired() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let out = btrdasd(&[
        "subvol", "expire", "--config", config.to_str().unwrap(),
        "--db", dir.path().join("index.db").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cd indexer && cargo test --test subvol_cli`
Expected: FAIL — `subvol_add_and_remove_regenerate_btrbk_conf` panics reading `btrbk.conf` (No such file), and `sync` / `expire` are unknown subcommands.

- [ ] **Step 3: Implement** in `main.rs`

Add to `SubvolAction`:

```rust
    /// Adopt subvolumes that exist and are not excluded, retire entries
    /// whose subvolume is gone. Expects the source volumes to be mounted.
    Sync {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Print what would change and write nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete the backups of retired subvolumes that are past their window.
    /// Expects the targets to be mounted.
    Expire {
        /// Path to config.toml
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Path to SQLite database
        #[arg(long, default_value = DEFAULT_DB)]
        db: PathBuf,
        /// Print what would be deleted and delete nothing
        #[arg(long)]
        dry_run: bool,
    },
```

A shared writer, used by all four editing handlers in place of their `std::fs::write(&config, cfg.to_toml()?)`:

```rust
/// Save an edited config and regenerate btrbk.conf from it, or change
/// neither. An entry that is in config.toml but not in btrbk.conf is not
/// backed up, however it looks.
fn save_config_and_btrbk_conf(cfg: &Config, config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let errors = cfg.validate();
    if !errors.is_empty() {
        return Err(errors.join("\n").into());
    }
    let conf_path = Path::new(&cfg.general.btrbk_conf);
    let previous = std::fs::read_to_string(conf_path).ok();
    buttered_dasd::fsutil::write_atomic(conf_path, &buttered_dasd::btrbk_conf::render_btrbk_conf(cfg))?;
    if let Err(e) = cfg.save(config_path) {
        if let Some(text) = previous {
            buttered_dasd::fsutil::write_atomic(conf_path, &text)?;
        }
        return Err(e);
    }
    Ok(())
}
```

Handlers:

```rust
            SubvolAction::Sync { config, dry_run } => {
                let outcome = match buttered_dasd::adopt::sync_subvolumes(
                    &config,
                    dry_run,
                    &buttered_dasd::caldate::today(),
                    &buttered_dasd::fsutil::SystemRunner,
                    &buttered_dasd::health::is_mountpoint,
                ) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(2);
                    }
                };
                print!("{}", buttered_dasd::adopt::format_sync_report(&outcome, dry_run));
                if outcome.failed() {
                    std::process::exit(1);
                }
            }
            SubvolAction::Expire { config, db, dry_run } => {
                let outcome = match buttered_dasd::expire::expire_retired(
                    &config,
                    dry_run,
                    &buttered_dasd::caldate::today(),
                    &buttered_dasd::fsutil::SystemRunner,
                    &buttered_dasd::health::is_mountpoint,
                ) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(2);
                    }
                };
                print!("{}", buttered_dasd::expire::format_expire_report(&outcome, dry_run));
                let mut failed = outcome.failed();
                let deleted: Vec<String> = outcome
                    .deleted_paths()
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                if !deleted.is_empty() {
                    // The index must not keep rows for snapshots that are gone.
                    match Database::open(&db).and_then(|database| {
                        let ids: Vec<i64> = database
                            .list_snapshots()?
                            .into_iter()
                            .filter(|s| deleted.contains(&s.path))
                            .map(|s| s.id)
                            .collect();
                        database.prune_snapshots(&ids)
                    }) {
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("Warning: deleted snapshots could not be removed from the index: {e}");
                            failed = true;
                        }
                    }
                }
                if failed {
                    std::process::exit(1);
                }
            }
```

In the `Backup → Run` handler, immediately after `let mut source_guard = mount::ensure_sources_mounted(&cfg, &progress);`, shadow `cfg` with the synced config:

```rust
                // The manual path gets the same guarantee as the scheduled
                // one: a subvolume that exists is backed up by this run.
                let mut sync_failed = false;
                match buttered_dasd::adopt::sync_subvolumes(
                    &config,
                    dry_run,
                    &buttered_dasd::caldate::today(),
                    &buttered_dasd::fsutil::SystemRunner,
                    &buttered_dasd::health::is_mountpoint,
                ) {
                    Ok(outcome) => {
                        eprint!("{}", buttered_dasd::adopt::format_sync_report(&outcome, dry_run));
                        sync_failed = outcome.failed();
                    }
                    Err(e) => {
                        eprintln!("Subvolume sync could not run: {e}");
                        sync_failed = true;
                    }
                }
                let cfg = Config::load(&config)?;
```

and where the handler decides its exit status, treat `sync_failed` like `!result.success` (exit 1 after the run has finished and been recorded).

- [ ] **Step 4: Run** `cd indexer && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` → PASS, including the four new CLI tests.

- [ ] **Step 5: Mutation gate, then commit**

```bash
git add indexer/src/main.rs indexer/tests/subvol_cli.rs
git commit -m "CLI: subvol sync and subvol expire; subvol add/remove now regenerate btrbk.conf

'subvol add' used to write config.toml and stop, leaving the subvolume
out of btrbk.conf until setup --upgrade was also run. The editing
commands now validate, save atomically and regenerate btrbk.conf, or
change neither file. The manual backup path runs sync before btrbk.

bd: DAS-Backup-Manager-gte"
```

---

### Task 11: The drift check becomes an alarm on sync

**Files:**
- Modify: `indexer/src/doctor.rs` — remove `REBUILDABLE_PATTERNS` (59), `DriftCategory` (228-237), `categorize` (241-248), `is_builtin_excluded` (215), `is_user_excluded` (223), `parse_subvolume_list_output` (253), `list_mounted_subvolumes` (434); rewrite `compute_missing` (277), `compute_stale` (295), `perform_drift_check` (463), `format_report` (568); fix the stale lock doc comment (117-128)
- Modify: `indexer/src/main.rs` tests at 2807 and 2851 (`MissingSubvolume` literals)

**Interfaces:**
- Consumes: `adopt::{in_snapshot_tree, excluding_pattern, list_volume, VolumeListing}`, `Config::exclude_patterns()`, `fsutil::SystemRunner`
- Produces:
  - `pub fn doctor::compute_missing(on_disk: &[String], configured: &[String], exclude: &[String]) -> Vec<String>`
  - `pub struct doctor::MissingSubvolume { pub volume: String, pub source_labels: Vec<String>, pub name: String }` (no `category`)
  - `group_sources_by_volume` puts only **non-retired** entries in `configured`
  - `glob_match` stays in `doctor.rs` and stays `pub`

- [ ] **Step 1: Write the failing tests** — replace the `categorize_*` tests, `compute_missing_categorizes_rebuildable_separately` and `format_report_separates_rebuildable_from_irreplaceable` in `doctor.rs` with:

```rust
    #[test]
    fn compute_missing_uses_the_same_rules_as_sync() {
        let on_disk = s(&["@", "@home", "@cache/stremio", "@steam", ".snapshots/1/snapshot", "@tmp", "new"]);
        let configured = s(&["@", "@home"]);
        let exclude = s(&["@tmp", "@var-tmp", "@cache"]);
        // "@steam" is reported: a name that looks rebuildable is no longer a
        // reason to leave a subvolume out.
        assert_eq!(compute_missing(&on_disk, &configured, &exclude), s(&["@steam", "new"]));
    }

    #[test]
    fn a_retired_entry_is_neither_configured_nor_stale() {
        let mut cfg = test_config();
        cfg.sources[0].subvolumes[0].retired = Some("2026-10-01".into());
        let gone = cfg.sources[0].subvolumes[0].name.clone();
        let groups = group_sources_by_volume(&cfg);
        assert!(!groups[0].configured.contains(&gone));
    }

    #[test]
    fn format_report_names_missing_as_a_sync_failure_and_suggests_no_hand_edit() {
        let report = DriftReport {
            volumes_checked: 1,
            volumes_failed: Vec::new(),
            missing: vec![MissingSubvolume {
                volume: "/.btrfs-hdd".into(),
                source_labels: vec!["hdd-media".into()],
                name: "bosco-media/video".into(),
            }],
            stale: Vec::new(),
        };
        let text = format_report(&report);
        assert!(text.contains("  Status: DRIFT DETECTED — FAILURE\n"), "{text}");
        assert!(
            text.contains(
                "NOT BACKED UP — the backup run should have adopted these\n"
            ),
            "{text}"
        );
        assert!(text.contains("  bosco-media/video  (volume /.btrfs-hdd, source(s): hdd-media)\n"), "{text}");
        assert!(text.contains("sudo btrdasd subvol sync --dry-run"), "{text}");
        assert!(!text.contains("SUGGESTED config.toml ADDITIONS"), "{text}");
        assert!(!text.contains("REBUILDABLE"), "{text}");
    }
```

(`s` is the existing `&[&str] -> Vec<String>` helper in that test module; add it if absent.)

- [ ] **Step 2: Run and watch them fail** — `cd indexer && cargo test --lib doctor` → FAIL to compile (`compute_missing` returns tuples; `MissingSubvolume` wants `category`).

- [ ] **Step 3: Implement**

```rust
/// On-disk subvolumes that sync should have adopted: not configured, not in
/// a snapshot tree, not excluded.
pub fn compute_missing(on_disk: &[String], configured: &[String], exclude: &[String]) -> Vec<String> {
    let mut missing: Vec<String> = on_disk
        .iter()
        .filter(|p| !crate::adopt::in_snapshot_tree(p))
        .filter(|p| crate::adopt::excluding_pattern(p, exclude).is_none())
        .filter(|p| !configured.contains(p))
        .cloned()
        .collect();
    missing.sort();
    missing.dedup();
    missing
}
```

- In `group_sources_by_volume`, build `configured` from `src.subvolumes.iter().filter(|e| e.retired.is_none())`. `compute_stale` is unchanged and therefore no longer reports retired entries.
- In `perform_drift_check`, replace `list_mounted_subvolumes(&group.volume)` with:

```rust
        let device = config
            .sources
            .iter()
            .find(|s| s.volume == group.volume)
            .map(|s| s.device.as_str())
            .unwrap_or("");
        let listing = crate::adopt::list_volume(
            &crate::fsutil::SystemRunner,
            &health::is_mountpoint,
            &group.volume,
            device,
        );
```

  and match on `listing.subvolumes` (`Ok(on_disk)` / `Err(why)` → `volumes_failed`). Pass `&config.exclude_patterns()` to `compute_missing`.
- In `format_report`: one MISSING section headed `NOT BACKED UP — the backup run should have adopted these`, each line `  {name}  (volume {volume}, source(s): {labels})`, followed by:

```
  The backup run adopts new subvolumes by itself. One appearing here means
  that step failed or has not run since the subvolume was created. See what
  it would do with:  sudo btrdasd subvol sync --dry-run
```

  Delete the "SUGGESTED config.toml ADDITIONS" block. Rename the stale section heading to `STALE CONFIG ENTRIES (configured, not retired, not found on disk)`.
- Replace the doc comment at 117-128 that says manual backups take no lock with one sentence: manual backups take the same locks through `backup::acquire_manual_locks` (bd `pe6`).
- Delete the items listed under **Files**; move `list_mounted_subvolumes_rejects_non_mountpoint` to assert the same through `adopt::list_volume`; update the two `main.rs` test literals.

- [ ] **Step 4: Run** `cd indexer && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` → PASS.

- [ ] **Step 5: Mutation gate, then commit**

```bash
git add indexer/src/doctor.rs indexer/src/main.rs
git commit -m "doctor: report a missing subvolume as a sync failure, not a to-do

The drift check uses the same listing and exclusion rules as sync. The
name heuristic and the 'suggested config.toml additions' block are gone:
a subvolume is left out only by an explicit pattern, and the fix for a
missing one is no longer a hand edit. Retired entries are not stale.

bd: DAS-Backup-Manager-gte"
```

---

### Task 12: `backup-run.sh` — sync before btrbk, expire after

**Files:**
- Modify: `scripts/backup-run.sh` — top-level config load (232-308), `main()` (2202-2203 and after 2214), `generate_report` heredoc (1678-1739), version string (1735, `v4.5.0` → `v4.6.0`)
- Create: `tests/test_subvol_sync.sh`
- Modify: `CMakeLists.txt:238-250` (register the new test)

**Interfaces:**
- Consumes: the CLI contract from Task 10
- Produces: bash functions `load_config_env`, `sync_subvolumes <mode>`, `expire_retired_subvolumes <mode>`; globals `SUBVOL_SYNC_REPORT`, `SUBVOL_EXPIRE_REPORT`; `OP_STATUS[subvol_sync]`, `OP_STATUS[subvol_expire]`

- [ ] **Step 1: Write the failing test** (`tests/test_subvol_sync.sh`), following the extract-by-`sed` pattern of `tests/test_verify_sources.sh`:

```bash
#!/bin/bash
# sync_subvolumes / expire_retired_subvolumes / load_config_env from
# scripts/backup-run.sh, exercised against a stub btrdasd. Both directions:
# a clean sync records OK and reloads the config; a failing one records FAIL,
# keeps the run going, and still reloads.
set -euo pipefail

SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in load_config_env sync_subvolumes expire_retired_subvolumes record_op; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || { echo "FAIL: $fn not found in backup-run.sh"; exit 1; }
    eval "$body"
done

log_info() { :; }; log_warn() { echo "WARN: $*" >>"$WORK/log"; }; log_error() { echo "ERROR: $*" >>"$WORK/log"; }
declare -A OP_STATUS=()
DAS_CONFIG="$WORK/config.toml"
DAS_DB_PATH="$WORK/index.db"
BTRDASD_BIN="$WORK/btrdasd"

# Stub: behaviour chosen by files in $WORK.
cat >"$BTRDASD_BIN" <<'STUB'
#!/bin/bash
here="$(dirname "$0")"
echo "$*" >>"$here/calls"
case "$1 $2" in
    "config dump-env")
        n=$(cat "$here/source_count")
        echo "DAS_SOURCE_COUNT=$n"
        for ((i = 0; i < n; i++)); do
            echo "DAS_SOURCE_${i}_LABEL='src$i'"
            echo "DAS_SOURCE_${i}_VOLUME='/vol$i'"
            echo "DAS_SOURCE_${i}_DEVICE='UUID=u$i'"
            echo "DAS_SOURCE_${i}_SNAPSHOT_DIR='.btrbk-snapshots'"
        done
        echo "DAS_TARGET_COUNT=0"
        echo "DAS_BTRBK_CONF='/etc/btrbk/btrbk.conf'"
        ;;
    "subvol sync")   cat "$here/sync_out";   exit "$(cat "$here/sync_rc")" ;;
    "subvol expire") cat "$here/expire_out"; exit "$(cat "$here/expire_rc")" ;;
esac
STUB
chmod +x "$BTRDASD_BIN"

fails=0
check() { if [[ "$2" == "$3" ]]; then echo "ok   $1"; else echo "FAIL $1: got '$2', want '$3'"; fails=$((fails + 1)); fi; }

# --- load_config_env rebuilds the per-source arrays from scratch ------------
echo 2 >"$WORK/source_count"
load_config_env
check "two sources loaded" "${#SOURCE_VOLUMES[@]}" "2"
echo 3 >"$WORK/source_count"
load_config_env
check "third source appears after reload" "${SOURCE_VOLUMES[src2]:-}" "/vol2"
echo 1 >"$WORK/source_count"
load_config_env
check "removed sources do not linger" "${#SOURCE_VOLUMES[@]}" "1"

# --- sync: clean ------------------------------------------------------------
: >"$WORK/calls"
printf 'SUBVOLUME SYNC\n  Adopted (now backed up):\n    @new\n' >"$WORK/sync_out"; echo 0 >"$WORK/sync_rc"
echo 2 >"$WORK/source_count"
sync_subvolumes run
check "clean sync records OK" "${OP_STATUS[subvol_sync]}" "OK"
check "report captured" "$(head -n1 <<<"$SUBVOL_SYNC_REPORT")" "SUBVOLUME SYNC"
check "config reloaded after sync" "${#SOURCE_VOLUMES[@]}" "2"
check "real run passes no --dry-run" "$(grep -c -- '--dry-run' "$WORK/calls" || true)" "0"

# --- sync: failure does not stop the run -----------------------------------
printf 'SUBVOLUME SYNC\n  VOLUMES NOT READ\n' >"$WORK/sync_out"; echo 1 >"$WORK/sync_rc"
rc=0; sync_subvolumes run || rc=$?
check "failing sync returns 0 so the backup continues" "$rc" "0"
check "failing sync records FAIL" "${OP_STATUS[subvol_sync]}" "FAIL"
check "failure detail names the exit code" "${OP_STATUS[subvol_sync_detail]}" "exit code 1 — see SUBVOLUME SYNC in the report"
check "report still captured on failure" "$(sed -n 2p <<<"$SUBVOL_SYNC_REPORT")" "  VOLUMES NOT READ"

# --- sync: dry run ----------------------------------------------------------
: >"$WORK/calls"; echo 0 >"$WORK/sync_rc"
sync_subvolumes dryrun
check "dry run passes --dry-run" "$(grep -c -- 'subvol sync .*--dry-run' "$WORK/calls")" "1"

# --- expire -----------------------------------------------------------------
: >"$WORK/calls"
printf 'RETIRED SUBVOLUMES\n  @opt\n' >"$WORK/expire_out"; echo 0 >"$WORK/expire_rc"
expire_retired_subvolumes run
check "clean expire records OK" "${OP_STATUS[subvol_expire]}" "OK"
check "expire passes the db" "$(grep -c -- "--db $DAS_DB_PATH" "$WORK/calls")" "1"
echo 1 >"$WORK/expire_rc"
rc=0; expire_retired_subvolumes run || rc=$?
check "failing expire returns 0" "$rc" "0"
check "failing expire records FAIL" "${OP_STATUS[subvol_expire]}" "FAIL"
: >"$WORK/expire_out"; echo 0 >"$WORK/expire_rc"
expire_retired_subvolumes run
check "empty expire report is empty" "$SUBVOL_EXPIRE_REPORT" ""

[[ $fails -eq 0 ]] && echo "ALL SUBVOL SYNC SHELL TESTS PASSED" || { echo "$fails FAILED"; exit 1; }
```

- [ ] **Step 2: Run and watch it fail**

Run: `bash tests/test_subvol_sync.sh`
Expected: `FAIL: load_config_env not found in backup-run.sh`, exit 1.

- [ ] **Step 3: Implement** in `scripts/backup-run.sh`

Turn the top-level config load into a function and call it once at top level, so behaviour at startup is unchanged. Everything from the `eval "$("$BTRDASD_BIN" config dump-env …)"` line through the construction of `ALL_TARGET_MOUNTS` (lines 238-308) moves inside, with each associative array re-declared so a reload starts empty:

```bash
# Load configuration from config.toml via btrdasd. Called once at startup and
# again after sync_subvolumes, because sync may add a source (the adoption
# source) and every per-source array below is built from this output.
load_config_env() {
    local env_text
    if ! env_text="$("$BTRDASD_BIN" config dump-env --config "$DAS_CONFIG")"; then
        echo "ERROR: btrdasd could not read $DAS_CONFIG" >&2
        return 1
    fi
    eval "$env_text"

    declare -gA DAS_SERIALS=() TARGET_MOUNT_UUIDS=() TARGET_MOUNTS=() TARGET_NAMES=() \
        TARGET_ROLES=() MOUNT_ROLES=() SOURCE_VOLUMES=() SOURCE_DEVICES=() SOURCE_SNAPSHOT_DIRS=()
    DAS_SERIALS_LIST=()
    ALL_TARGET_MOUNTS=()
    # … the existing loops that fill these arrays, unchanged …
}

if [[ -x "$BTRDASD_BIN" ]]; then
    load_config_env || exit 1
else
    echo "ERROR: btrdasd not found at $BTRDASD_BIN" >&2
    exit 1
fi
```

Keep the existing loop bodies byte-for-byte; only their indentation and the `declare -gA` line change. Check with `shellcheck` that no array is still declared outside the function.

Add after `verify_sources_before_write`'s definition:

```bash
SUBVOL_SYNC_REPORT=""
SUBVOL_EXPIRE_REPORT=""

# Adopt subvolumes that exist and are not excluded; retire entries whose
# subvolume is gone. Runs after verify_sources_before_write, so every source
# volume is mounted and proven to be the expected filesystem.
#
# A failure here never stops the backup: the subvolumes already configured
# must still be backed up. It is recorded, so the report says FAILURES
# DETECTED and backup_runs.success is 0.
sync_subvolumes() {
    local mode="$1"
    local args=(subvol sync --config "$DAS_CONFIG")
    [[ "$mode" == "dryrun" ]] && args+=(--dry-run)

    log_info "Syncing subvolumes with config..."
    local rc=0
    SUBVOL_SYNC_REPORT="$("$BTRDASD_BIN" "${args[@]}")" || rc=$?
    if [[ $rc -eq 0 ]]; then
        record_op "subvol_sync" "OK"
    else
        record_op "subvol_sync" "FAIL" "exit code $rc — see SUBVOLUME SYNC in the report"
        log_error "Subvolume sync failed (exit $rc); continuing with the existing config"
    fi
    while IFS= read -r line; do log_info "  $line"; done <<<"$SUBVOL_SYNC_REPORT"

    # Sync may have added a source. Reload even after a failure: a partial
    # success on one volume still changed the config.
    if ! load_config_env; then
        record_op "subvol_sync" "FAIL" "config could not be reloaded after sync"
        log_error "Config could not be reloaded after subvolume sync"
    fi
    return 0
}

# Delete the backups of retired subvolumes that are past their window.
# Runs after btrbk, while the targets are still mounted.
expire_retired_subvolumes() {
    local mode="$1"
    local args=(subvol expire --config "$DAS_CONFIG" --db "$DAS_DB_PATH")
    [[ "$mode" == "dryrun" ]] && args+=(--dry-run)

    local rc=0
    SUBVOL_EXPIRE_REPORT="$("$BTRDASD_BIN" "${args[@]}")" || rc=$?
    if [[ $rc -eq 0 ]]; then
        record_op "subvol_expire" "OK"
    else
        record_op "subvol_expire" "FAIL" "exit code $rc — see RETIRED SUBVOLUMES in the report"
        log_error "Expiry of retired subvolumes failed (exit $rc)"
    fi
    if [[ -n "$SUBVOL_EXPIRE_REPORT" ]]; then
        while IFS= read -r line; do log_info "  $line"; done <<<"$SUBVOL_EXPIRE_REPORT"
    fi
    return 0
}
```

In `main()`:

```bash
    verify_sources_before_write
    sync_subvolumes "$mode"
    create_snapshot_dirs
```

and directly after `run_btrbk "$mode"` (before the `if [[ "$mode" != "dryrun" ]]` block that follows it):

```bash
    expire_retired_subvolumes "$mode"
```

In `generate_report`'s heredoc, add to the BACKUP OPERATIONS list two lines in the same format as its neighbours (`Subvolume sync` → `${OP_STATUS[subvol_sync]:-N/A}`, `Retired expiry` → `${OP_STATUS[subvol_expire]:-N/A}`), and after that list, before THROUGHPUT:

```
$SUBVOL_SYNC_REPORT
$SUBVOL_EXPIRE_REPORT
```

Bump the footer version to `backup-run.sh v4.6.0`.

Register the test next to the existing four in `CMakeLists.txt`:

```cmake
add_test(NAME subvol-sync-shell
         COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_subvol_sync.sh)
```

- [ ] **Step 4: Run and watch it pass**

```bash
bash tests/test_subvol_sync.sh
shellcheck scripts/*.sh tests/*.sh
for t in tests/test_*.sh; do bash "$t" >/dev/null && echo "ok $t" || echo "FAIL $t"; done
```
Expected: `ALL SUBVOL SYNC SHELL TESTS PASSED`; shellcheck clean; all five test scripts `ok` (the other four must still pass — `test_verify_sources.sh` and `test_create_snapshot_dirs.sh` stub the arrays that moved into `load_config_env`).

- [ ] **Step 5: Falsify the test**

Temporarily change `record_op "subvol_sync" "FAIL"` to `"OK"` in `sync_subvolumes`, run `bash tests/test_subvol_sync.sh`, and confirm it prints `FAIL failing sync records FAIL`. Restore the line and re-run to green.

- [ ] **Step 6: Commit**

```bash
git add scripts/backup-run.sh tests/test_subvol_sync.sh CMakeLists.txt
git commit -m "backup-run.sh 4.6.0: sync subvolumes before btrbk, expire retired ones after

The run now adopts new subvolumes and retires vanished ones by itself,
between verifying the sources and creating snapshot directories, and
reloads its config afterwards because sync can add a source. A sync or
expiry failure is recorded and reported; it never stops the backup of
what is already configured.

bd: DAS-Backup-Manager-gte"
```

---

### Task 13: CI scope, documentation, changelog

**Files:**
- Modify: `.github/workflows/mutants.yml:168-178` and the comment at ~151
- Modify: `.claude/rules/backup.md` (section at line 25), `.claude/rules/build.md` (module-count line)
- Modify: `docs/INSTALL.md` (335-343), `docs/btrdasd.1` (539-565, 717-758), `docs/ARCHITECTURE.md` (336, 521), `README.md` (30, 63)
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Widen the weekly mutation scope**

Add to the `cargo mutants` invocation in the `full-scope` job:

```yaml
            --file src/adopt.rs \
            --file src/expire.rs \
            --file src/btrbk_conf.rs \
            --file src/caldate.rs \
            --file src/fsutil.rs \
```

Measure, and write the measured figure into the comment that currently reads `Measured 2026-10-01: 500 mutants`:

```bash
cd indexer && env -u CARGO_TARGET_DIR cargo mutants --list \
  --file 'src/setup/**/*.rs' --file src/mount.rs --file src/forget.rs --file src/restore.rs \
  --file src/config.rs --file src/adopt.rs --file src/expire.rs --file src/btrbk_conf.rs \
  --file src/caldate.rs --file src/fsutil.rs | wc -l
```

Then run that full scope on a copy of the repo and score it; it must be `OK` with 0 missed before this task is committed.

- [ ] **Step 2: Rewrite `.claude/rules/backup.md` §"Nested Subvolumes Need Their Own Config Entry — Always"**

Replace the section's first bullet and add two, keeping the rest:

```markdown
- **`btrfs send` does not descend into nested subvolumes.** A parent's snapshot holds an empty
  directory where a child subvolume sits, and the run reports success. Every subvolume still
  needs its own entry — and **the backup run now writes it** (`btrdasd subvol sync`, called by
  `backup-run.sh` before btrbk and by `btrdasd backup run`). Creating a subvolume is enough.
- A subvolume is left out only by `[subvolumes].exclude` (a pattern also covers everything
  nested under it) or by sitting in a `.snapshots` / `.btrbk-snapshots` tree. Every skip is
  listed in the run report with its pattern.
- An entry whose subvolume is gone is **retired**, not an error: it leaves `btrbk.conf`, and
  its backups are deleted from each target once the retirement date plus that target's longest
  retention window has passed. A target with no retention keeps them and says so.
```

Delete the bullet beginning "**The reverse is loud, not silent**" (a dead entry no longer makes btrbk exit 10) and replace it with: `- A **Missing** or **Stale** finding from the weekly drift check now means sync itself failed.`

Check the instruction-file budget afterwards: `cat ~/.claude/CLAUDE.md ~/.claude/rules/*.md ../CLAUDE.md CLAUDE.md .claude/rules/*.md | wc -m` must not have grown by more than the bullets added; move reasoning to `.claude/docs/rules-reference/backup.md` if it has.

- [ ] **Step 3: Update the other docs**

- `docs/INSTALL.md`: add a `### [subvolumes]` section documenting `exclude` (with the nested-coverage rule and the defaults `@tmp`, `@var-tmp`); mark `[doctor].exclude` as still read and merged; document `adopted` / `retired` on `[[source.subvolumes]]`.
- `docs/btrdasd.1`: add `.SS subvol sync` and `.SS subvol expire` with their exit codes (0 / 1 / 2 as in Task 10); update `.SS subvol add` and `remove` to say they regenerate `btrbk.conf`; update `.SS doctor`.
- `docs/ARCHITECTURE.md`: add rows for `adopt.rs`, `expire.rs`, `btrbk_conf.rs`, `caldate.rs`, `fsutil.rs`; add `[subvolumes]` to the config table.
- `README.md` line 30: the drift detector no longer offers a "ready-to-paste config diff"; say new subvolumes are adopted by the backup run. Line 63: fix the module list from `grep '^pub mod ' indexer/src/lib.rs`.
- `.claude/rules/build.md`: update the module count and list the same way.

- [ ] **Step 4: Changelog** — under `## [Unreleased]`:

`### Added`
```markdown
- **The backup run adopts new subvolumes by itself** (bd `DAS-Backup-Manager-gte`) — `btrdasd subvol sync`, called by `backup-run.sh` before btrbk and by `btrdasd backup run`, adds an entry for every subvolume that exists on a source volume and is not excluded. A nested subvolume takes its parent's source and targets; one with no configured parent goes to the primary target only, through a per-volume `<source>-adopted` source. Each adopted entry gets an explicit, unique `snapshot_name` and an `adopted` date, and is listed in the run report
- **Retired subvolumes** — an entry whose subvolume is gone is marked `retired` and leaves `btrbk.conf`; `btrdasd subvol expire` deletes its backups from each target once the retirement date plus that target's longest retention window has passed, and removes the entry when none remain. A target that is not attached, or has no retention configured, is reported and left alone
- **`[subvolumes].exclude`** — the one list of what is left out. A pattern also covers everything nested under what it matches. `[doctor].exclude` is still read and merged
```

`### Changed`
```markdown
- **`btrdasd subvol add` / `remove` / `set-manual` / `set-auto` regenerate `btrbk.conf`** and validate before saving — previously `subvol add` wrote `config.toml` only, so the subvolume was not backed up until `setup --upgrade` was also run
- **A missing subvolume in the weekly drift check now means sync failed** — the name-based "probably rebuildable" category and the "suggested config.toml additions" block are removed
- **`config.toml` and `btrbk.conf` are replaced atomically** (temp file, then rename)
- **`Config::validate()` rejects two entries that resolve to one snapshot name** in the same snapshot directory
```

`### Fixed`
```markdown
- **Subvolume paths containing a space were misread** — the listing parser took the last word of the path
- **`btrdasd backup run` did not check that a source volume held the expected filesystem** before using it; it now verifies the UUID and that the volume is mounted at its top level, as `backup-run.sh` already did
```

- [ ] **Step 5: Lint and commit**

```bash
git ls-files -z -- . ':!.beads' | xargs -0 uvx codespell@2.4.3
yamllint -d '{extends: default, rules: {line-length: disable, document-start: disable, truthy: {check-keys: false}, comments: {min-spaces-from-content: 1}}}' .github/workflows/mutants.yml
git add -A
git commit -m "Docs, changelog and weekly mutation scope for subvolume sync

bd: DAS-Backup-Manager-gte"
```

---

### Task 14: End-to-end proof, both directions, then deploy

**Files:**
- Create: `indexer/tests/subvol_sync_loopback.rs`

This task needs root and is the only one that may use it. The loopback test runs on the VM `test-vm-cachyos` (use the `/vm-exec` skill), never on the host.

- [ ] **Step 1: Write the loopback test**, modelled on `indexer/tests/boot_archive_loopback.rs` (copy its `is_root`, `run`, `must` and loop-device rig; `#[ignore = "requires root and loop devices"]`). One rig: a source BTRFS and a target BTRFS, each on a loop device, mounted at tempdirs; a config with one source (`@data`) and one primary target (`daily = 7`); `general.btrbk_conf` in the tempdir. Two tests:

```rust
#[test]
#[ignore = "requires root and loop devices"]
fn a_subvolume_created_after_setup_is_backed_up_by_the_next_run() {
    let rig = Rig::new();                       // source has @data only
    rig.btrfs(&["subvolume", "create", &rig.src("@data/nested")]);
    rig.btrfs(&["subvolume", "create", &rig.src("brand-new")]);
    std::fs::write(rig.src("@data/nested/file"), b"payload").unwrap();

    // Counter-test first: btrbk on the config as it stands does not carry them.
    rig.btrbk_run();
    assert!(rig.target_snapshots("src").iter().all(|n| n.starts_with("data.")));

    let outcome = buttered_dasd::adopt::sync_subvolumes(
        &rig.config_path, false, "2026-10-02",
        &buttered_dasd::fsutil::SystemRunner, &buttered_dasd::health::is_mountpoint,
    ).unwrap();
    assert!(outcome.written && !outcome.failed(), "{outcome:?}");
    rig.btrbk_run();

    let nested = rig.target_snapshots("src");
    assert!(nested.iter().any(|n| n.starts_with("data-nested.")), "{nested:?}");
    let adopted = rig.target_snapshots("src-adopted");
    assert!(adopted.iter().any(|n| n.starts_with("brand-new.")), "{adopted:?}");
    // The file inside the nested subvolume really arrived.
    let snap = nested.iter().find(|n| n.starts_with("data-nested.")).unwrap();
    assert_eq!(std::fs::read(rig.tgt(&format!("src/{snap}/file"))).unwrap(), b"payload");
}

#[test]
#[ignore = "requires root and loop devices"]
fn a_deleted_subvolume_is_retired_and_its_backups_expire_after_the_window() {
    let rig = Rig::new();
    rig.btrfs(&["subvolume", "create", &rig.src("doomed")]);
    rig.sync("2026-10-02");
    rig.btrbk_run();
    assert!(!rig.target_snapshots("src-adopted").is_empty());

    rig.btrfs(&["subvolume", "delete", &rig.src("doomed")]);
    let outcome = rig.sync("2026-10-03");
    assert_eq!(outcome.plan.retire.len(), 1);
    rig.btrbk_run(); // must not exit 10 on the retired entry

    // Inside the 7-day window: kept.
    rig.expire("2026-10-10");
    assert!(!rig.target_snapshots("src-adopted").is_empty());
    // Past it: gone, and the entry with it.
    rig.expire("2026-10-11");
    assert!(rig.target_snapshots("src-adopted").is_empty());
    let cfg = buttered_dasd::config::Config::load(&rig.config_path).unwrap();
    assert!(cfg.sources.iter().all(|s| s.subvolumes.iter().all(|e| e.name != "doomed")));
}
```

`Rig::btrbk_run` runs `btrbk -c <conf> run` and asserts exit 0. `Rig::sync` / `Rig::expire` call the library functions with `SystemRunner` and `health::is_mountpoint`.

- [ ] **Step 2: Run it on the VM**

```bash
sudo -E cargo test --test subvol_sync_loopback -- --ignored --nocapture --test-threads=1
```
Expected: `2 passed`. Record the output.

- [ ] **Step 3: Falsify it on the VM**

In `apply_plan`, comment out the line that pushes the adopted `SubvolConfig`; re-run; the first test must fail at `data-nested.`. Restore and re-run to green.

- [ ] **Step 4: Run the real script on the VM** — install the build, create a loopback source and target as `btrdasd setup` would configure them, create a new subvolume, run `/usr/lib/das-backup/backup-run.sh`, and confirm: the report contains `SUBVOLUME SYNC` with `Adopted (now backed up):`, the snapshot is on the target, `Status: ALL OPERATIONS SUCCESSFUL`. Then the counter-test: unmount the source before the run so sync cannot read it, and confirm the report says `FAILURES DETECTED` and `VOLUMES NOT READ`, with the configured subvolumes on the other volumes still backed up. Revert the VM to its `pristine` snapshot afterwards.

- [ ] **Step 5: Commit the test and push**

```bash
git add indexer/tests/subvol_sync_loopback.rs
git commit -m "Root end-to-end test: a new subvolume is backed up by the next run

bd: DAS-Backup-Manager-gte"
~/.claude/bin/scrub-promo check origin/main..HEAD
git push origin main
```
Wait for CI, Semgrep, CodeQL and the Mutants push job to succeed, and read the gate line from the Mutants job log (`mutants [...] OK`).

- [ ] **Step 6: Deploy on the host**

Only when `systemctl is-active das-backup.service das-backup-full.service das-scrub.service` prints three `inactive`:

```bash
B=/tmp/das-backup-manager-build
env -u CARGO_TARGET_DIR cmake -S . -B "$B" -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr
env -u CARGO_TARGET_DIR cmake --build "$B" -j
sudo cmake --install "$B"
sudo btrdasd setup --upgrade
sudo btrdasd config validate
sudo systemctl restart btrdasd-helper.service
```

- [ ] **Step 7: Prove it on the host**

1. Dry run against the live volumes (they must be mounted; `backup-run.sh --dryrun` does that and unmounts afterwards):

   `sudo /usr/lib/das-backup/backup-run.sh --dryrun 2>&1 | sed -n '/SUBVOLUME SYNC/,/^$/p'`

   Expected: nothing to adopt or retire (`bosco-media/video` and `@srv/stremio-web` were added by hand on 2026-10-01), and `@cache/stremio  [/.btrfs-ssd, excluded by '@cache']` under Skipped. **If the dry run proposes to retire anything, STOP** — that would be a listing bug, and it must be understood before a real run.
2. The live proof: `sudo btrfs subvolume create /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager/.sync-proof` (nested under a configured project subvolume), write a file into it, start `das-backup.service`, and when it finishes confirm from `/var/lib/das-backup/last-report.txt` that it was adopted, from the target that its snapshot exists with the file in it, and from `backup_runs` that the run succeeded.
3. Retire proof: delete `.sync-proof`, run again, confirm the report's `Retired` line and that btrbk did not fail. Its backups then expire on their own schedule; note the dates in bd `gte`.
4. `sudo btrdasd doctor --check-drift; echo $?` → `0`.

- [ ] **Step 8: Close out**

Append the evidence (commands and their result lines) to bd `gte`, close `gte` and `gya`, and add a note to bd `31q` that adopted entries and validation now cover it. Remove `/tmp/das-backup-manager-*`.

---

## Self-Review

**Spec coverage.** §5.1 units → Tasks 5-9. §5.2 placement, dry run, manual path, `subvol add` fix, atomic writes → Tasks 3, 7, 10, 12. §5.3 adopt (nested, non-nested, adoption source, explicit unique names, `adopted` date, validation) → Tasks 3, 6. §5.4 skip (snapshot tree, exclude covering nested, list moved, defaults, heuristic deleted, skips reported) → Tasks 3, 5, 6, 7, 11. §5.5 retire / expire / revive / report on every run → Tasks 6, 8, 9, 12. §5.6 failure table → Task 5 (empty listing), Task 7 (unreadable volume, failed write, invalid config), Task 6 (unplaceable), Task 9 (failed delete), Task 12 (run continues, marked failed). §5.7 drift check → Task 11. §5.8 schema → Task 3. §6 testing → every task plus Task 14. §7 docs → Task 13. §9 expiry rule → Tasks 8, 9.

**Gap found and closed while reviewing:** the spec says the report lists retired entries "on every run while any retired entry exists". `format_expire_report` returns text whenever an entry is retired, and Task 12 prints it into the report — covered.

**Type consistency.** `sync_subvolumes` and `expire_retired` take the same five arguments in the same order in Tasks 7, 9, 10 and 14. `SyncOutcome.failed()`, `ExpireOutcome.failed()` and `SyncPlan.failed()` are the three places "the run is marked failed" is decided. `Scripted` is defined once in `fsutil::testing` (Task 9 moves it from Task 7's test module). `compute_missing` returns `Vec<String>` from Task 11 on; nothing earlier depends on it.

**Known limits, stated rather than hidden.** The Rust code in this plan was written against an interface map of the tree at `fe2dc39`, not compiled. Where the compiler disagrees, fix the code to satisfy the tests as written; if a test itself contradicts the spec, stop and report it. Line numbers are as of `fe2dc39` and will drift as tasks land.
