# Recovery State Implementation Plan (plan 1 of 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every backup target carries an index that matches the data it received that night, and a recovery drive's own OS, at boot, has a current copy of btrdasd's state and a configuration that lets btrdasd, the helper and ButteredDASD restore from the drive itself — or from the 22 TB pair when it is attached, for what only the pair holds — with no manual step.

**Architecture:** (1) `/var/lib/das-backup` becomes its own nested subvolume, sent by a second small btrbk pass after indexing (spec D8). (2) `btrdasd config derive-recovery` turns the host's backed-up `config.toml` into a recovery-drive configuration with the real parser (spec D4, option C): no sources, every target kept with the booted drive's first, so an attached 22 TB pair is mountable by UUID as on the host. (3) A boot-time unit on the recovery OS reflinks the state out of the newest received `das-state` snapshot, derives the configuration from the newest received `root-` snapshot, and runs an incremental index catch-up only when the state is older than the data. (4) The helper resolves a snapshot to a copy that exists on this machine, so the GUI restores from the drive's own received snapshot when the primary target is absent.

**Tech Stack:** bash (`scripts/backup-run.sh`, new `scripts/das-recovery-state-sync.sh`), Rust 2024 (`indexer/src/config.rs`, `db.rs`, `main.rs`, `bin/btrdasd-helper.rs`), systemd unit, CMake install, the project's bash test style (`tests/test_record_run.sh`: extract functions, stub tools, `set -euo pipefail`), cargo tests with `cargo-mutants` in CI.

**Spec:** `docs/superpowers/specs/2026-10-12-recovery-drive-provisioning-design.md` §3.5, §4 Phase 3, §5, decisions D4 and D8.

## Global Constraints

- Rust 2024 edition; `cargo clippy -- -D warnings`, `cargo fmt`; every new function that spawns a command or reads the filesystem goes through a test-drivable seam or its mutants survive (`.claude/rules/build.md`).
- Bash scripts `#!/bin/bash`, `set -euo pipefail` where the suite expects it, `shellcheck -S warning` clean.
- Build with `cmake --build /tmp/das-backup-build` and `cargo test` with `CARGO_TARGET_DIR=/tmp/das-backup-target`; never a bare `cargo build`, never a target dir on the array.
- The recovery OS never runs btrbk, the backup timers or a scrub (spec §2.2). Nothing in this plan enables a timer anywhere.
- The host writes nothing to a recovery drive's ESP (spec §2.1). This plan writes only to btrfs targets through btrbk, as today.
- A failure branch never records success (`.claude/rules/fail-silent.md`): the state pass failing is a WARN with its reason; the sync unit failing leaves the previous copy and exits nonzero.
- `backup-run.sh` exit semantics stay (`.claude/rules/backup.md` §Sentinel): 0 clean or WARN, 3 FAIL, 1 could not start.
- No AI attribution in any commit (`~/.claude/rules/git-commits.md`).
- Version strings: `CMakeLists.txt` `project(VERSION)`; script headers carry their own `# Version:` line and are bumped when edited.
- `.staged-release` is present (0.7.23.0): commit pushes are fine, no tag or release is created by this plan.

## Review Focus

1. A recovery drive whose newest received `das-state` snapshot is **older** than its newest `root-` snapshot (a night when the state pass failed): the sync unit must copy the older state, derive the config, and start the catch-up index — never refuse, never leave an empty `/var/lib/das-backup`. Pinned in Task 5's "gap" test.
2. A drive whose p2 holds **no** `das-state` snapshot at all (first boot after provisioning, before the next backup): the unit must say so, leave whatever state exists untouched, exit nonzero, and the helper must still start. Pinned in Task 5's "no snapshot" test.
3. The state pass runs while a **target is absent** (one recovery drive unplugged): the filter for that target must not be passed to btrbk, and the pass must still run for the targets that are mounted. Pinned in Task 3's "absent target" test.
4. The helper asked for a snapshot whose **primary path exists but is stale** (the 22 TB mounted read-only from an older copy, a recovery drive booted beside it): the resolver prefers the path on the booted filesystem's own target when the configuration marks `recovery_drive = true`. Pinned in Task 1's "prefer own target in recovery" test.
5. `derive-recovery` given a `config.toml` whose targets name the booted UUID **twice** (a hand edit): refuse with both labels named, never pick one. Pinned in Task 2's "ambiguous target" test.

---

### Task 1: The helper resolves a snapshot to a copy that exists here

**Files:**

- Modify: `indexer/src/db.rs` (after `targets_for_snapshot`, ~line 848)
- Modify: `indexer/src/bin/btrdasd-helper.rs:1398-1416` (`IndexSnapshotPath`)
- Modify: `indexer/src/config.rs:131-145` (`General` gains `recovery_drive: bool`)
- Test: `indexer/src/db.rs` `mod tests`

**Interfaces:**

- Consumes: `Database::snapshot_path_by_id(id) -> SqlResult<Option<String>>`, `Database::targets_for_snapshot(id) -> SqlResult<Vec<(String, String)>>` (existing).
- Produces: `Database::snapshot_path_present(&self, id: i64, prefer_root: Option<&str>, exists: &dyn Fn(&Path) -> bool) -> SqlResult<Option<String>>`; `Config.general.recovery_drive: bool` (serde default `false`), used by Task 2 and Task 5.

- [ ] **Step 1: Write the failing tests**

In `indexer/src/db.rs` `mod tests` (use the module's existing `Database::open_in_memory()` or the temp-file helper the other tests use):

```rust
#[test]
fn snapshot_path_present_prefers_the_primary_path_when_it_exists() {
    let db = Database::open_in_memory().unwrap();
    let id = db.insert_snapshot("root-", "20261012T0348", "nvme", "/mnt/backup-22tb/nvme/root-.20261012T0348").unwrap();
    db.record_snapshot_target(id, "/mnt/backup-system-recovery-A", "/mnt/backup-system-recovery-A/nvme/root-.20261012T0348").unwrap();
    let exists = |_: &Path| true;
    assert_eq!(
        db.snapshot_path_present(id, None, &exists).unwrap().as_deref(),
        Some("/mnt/backup-22tb/nvme/root-.20261012T0348")
    );
}

#[test]
fn snapshot_path_present_falls_back_to_a_copy_that_exists() {
    let db = Database::open_in_memory().unwrap();
    let id = db.insert_snapshot("root-", "20261012T0348", "nvme", "/mnt/backup-22tb/nvme/root-.20261012T0348").unwrap();
    db.record_snapshot_target(id, "/mnt/backup-system-recovery-A", "/mnt/backup-system-recovery-A/nvme/root-.20261012T0348").unwrap();
    let exists = |p: &Path| p.starts_with("/mnt/backup-system-recovery-A");
    assert_eq!(
        db.snapshot_path_present(id, None, &exists).unwrap().as_deref(),
        Some("/mnt/backup-system-recovery-A/nvme/root-.20261012T0348")
    );
}

#[test]
fn snapshot_path_present_prefers_own_target_in_recovery() {
    // Review focus 4: primary present but stale beside a booted recovery drive.
    let db = Database::open_in_memory().unwrap();
    let id = db.insert_snapshot("root-", "20261012T0348", "nvme", "/mnt/backup-22tb/nvme/root-.20261012T0348").unwrap();
    db.record_snapshot_target(id, "/mnt/backup-system-recovery-A", "/mnt/backup-system-recovery-A/nvme/root-.20261012T0348").unwrap();
    let exists = |_: &Path| true;
    assert_eq!(
        db.snapshot_path_present(id, Some("/mnt/backup-system-recovery-A"), &exists).unwrap().as_deref(),
        Some("/mnt/backup-system-recovery-A/nvme/root-.20261012T0348")
    );
}

#[test]
fn snapshot_path_present_is_none_when_no_copy_exists() {
    let db = Database::open_in_memory().unwrap();
    let id = db.insert_snapshot("root-", "20261012T0348", "nvme", "/mnt/backup-22tb/nvme/root-.20261012T0348").unwrap();
    let exists = |_: &Path| false;
    assert_eq!(db.snapshot_path_present(id, None, &exists).unwrap(), None);
}

#[test]
fn snapshot_path_present_unknown_id_is_none() {
    let db = Database::open_in_memory().unwrap();
    let exists = |_: &Path| true;
    assert_eq!(db.snapshot_path_present(404, None, &exists).unwrap(), None);
}
```

If `insert_snapshot` has another name or signature in `db.rs`, use the existing insert the other tests use; do not add a second insert.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-backup-target cargo test --features dbus --lib -- snapshot_path_present`
Expected: compile error, `no method named snapshot_path_present`.

- [ ] **Step 3: Implement the resolver and the flag**

In `indexer/src/db.rs`, after `targets_for_snapshot`:

```rust
/// A path of `snapshot_id` that exists on this machine: the preferred
/// target's copy first (a recovery drive's own, `prefer_root`), then the
/// primary path the snapshot was indexed from, then every other recorded
/// copy in `target_root` order. `None` when nothing exists or the id is
/// unknown. `exists` is the probe (a seam for the tests): the real caller
/// passes `Path::is_dir`.
pub fn snapshot_path_present(
    &self,
    snapshot_id: i64,
    prefer_root: Option<&str>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> SqlResult<Option<String>> {
    let Some(primary) = self.snapshot_path_by_id(snapshot_id)? else {
        return Ok(None);
    };
    let copies = self.targets_for_snapshot(snapshot_id)?;
    if let Some(root) = prefer_root
        && let Some((_, p)) = copies.iter().find(|(r, _)| r == root)
        && exists(std::path::Path::new(p))
    {
        return Ok(Some(p.clone()));
    }
    if exists(std::path::Path::new(&primary)) {
        return Ok(Some(primary));
    }
    Ok(copies
        .into_iter()
        .map(|(_, p)| p)
        .find(|p| exists(std::path::Path::new(p))))
}
```

In `indexer/src/config.rs` `General` (keep field order; serde default):

```rust
/// This configuration was derived for a recovery drive's own OS
/// (`btrdasd config derive-recovery`): no sources, one target — the booted
/// filesystem — and restores allowed only under the recovery roots.
#[serde(default)]
pub recovery_drive: bool,
```

Add `recovery_drive: false` wherever `General` is constructed literally (grep `General {`); `Default` derives keep it false.

In `indexer/src/bin/btrdasd-helper.rs` `IndexSnapshotPath` (lines 1398-1416): load the config with `Config::load(Path::new(DEFAULT_CONFIG))` as the other methods do; compute `prefer_root` as `cfg.targets[0].mount` when `cfg.general.recovery_drive` is true (Task 2 puts the booted drive first), else `None`; replace the `snapshot_path_by_id` call with:

```rust
let path = db
    .snapshot_path_present(snapshot_id, prefer_root.as_deref(), &|p| p.is_dir())
    .map_err(|e| fdo::Error::Failed(format!("Path query failed: {e}")))?
    .ok_or_else(|| fdo::Error::Failed(format!(
        "No copy of snapshot {snapshot_id} is present on this machine (the target it was indexed from is not mounted, and no other target holding it is)"
    )))?;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-backup-target cargo test --features dbus --lib -- snapshot_path_present config::`
Expected: the five new tests PASS; every `config::` test still PASSES.

- [ ] **Step 5: Lint and commit**

```bash
cd indexer && cargo fmt && CARGO_TARGET_DIR=/tmp/das-backup-target cargo clippy --features dbus --all-targets -- -D warnings
cd .. && git add indexer/src/db.rs indexer/src/config.rs indexer/src/bin/btrdasd-helper.rs
git commit -m "helper: a snapshot resolves to a copy that exists here, the booted drive's first on a recovery OS"
```

---

### Task 2: `btrdasd config derive-recovery`

**Files:**

- Modify: `indexer/src/config.rs` (new `impl Config` fn after `validate`, ~line 858; `validate` itself at 763-770)
- Modify: `indexer/src/main.rs:414-440` (`ConfigAction`), `:2116-2140` (handler)
- Test: `indexer/src/config.rs` `mod tests`

**Interfaces:**

- Consumes: `Config::load`, `Config::to_toml`, `Config::validate`, `Target.mount_uuid: Option<String>`, `Target.mount: String`, `Restore.allowed_roots`, `Email.enabled`, `Scrub.enabled`, `Boot.enabled`, `General.recovery_drive` (Task 1).
- Produces: `Config::derive_recovery(&self, booted_uuid: &str) -> Result<Config, String>` — every target kept, the booted one first (`targets[0]`), so Task 1's helper can take `targets[0].mount` as the preferred root; CLI `btrdasd config derive-recovery --from <path> --booted-uuid <uuid> [--out <path>]` (prints the TOML to stdout when `--out` is absent; exit 1 with the reason on stderr when it refuses). Task 4 calls it.

Constants, in `config.rs`:

```rust
/// Where a recovery boot may restore to, and nowhere else (spec D4).
pub const RECOVERY_ALLOWED_ROOTS: [&str; 2] = ["/mnt/restore", "/home/restore-test"];
```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn derive_recovery_keeps_every_target_with_the_booted_one_first_and_drops_the_sources() {
    let cfg = test_config_with_three_targets(); // helper below
    let r = cfg.derive_recovery("7c7ae72d-09d6-4086-b249-1ac60f21b73b").unwrap();
    assert!(r.general.recovery_drive);
    assert!(r.sources.is_empty());
    let labels: Vec<&str> = r.targets.iter().map(|t| t.label.as_str()).collect();
    // the booted drive first, the others in their original order: the 22 TB
    // pair stays mountable by UUID for what only it holds
    assert_eq!(labels, vec!["system-recovery-B-2tb", "primary-22tb", "system-recovery-A-2tb"]);
    assert_eq!(r.targets[0].mount, "/mnt/backup-system-recovery-B");
    assert!(!r.email.enabled);
    assert!(!r.scrub.enabled);
    assert!(r.scrub.targets.is_empty());
    assert!(!r.boot.enabled);
    assert_eq!(r.restore.allowed_roots, vec!["/mnt/restore".to_string(), "/home/restore-test".to_string()]);
}

#[test]
fn derive_recovery_refuses_an_unknown_uuid_naming_the_targets() {
    let cfg = test_config_with_three_targets();
    let e = cfg.derive_recovery("00000000-0000-0000-0000-000000000000").unwrap_err();
    assert!(e.contains("no target of filesystem 00000000-0000-0000-0000-000000000000"), "{e}");
    assert!(e.contains("primary-22tb") && e.contains("system-recovery-A-2tb") && e.contains("system-recovery-B-2tb"), "{e}");
}

#[test]
fn derive_recovery_refuses_an_ambiguous_uuid() {
    // Review focus 5: two targets with the same mount_uuid (a hand edit).
    let mut cfg = test_config_with_three_targets();
    cfg.targets[0].mount_uuid = cfg.targets[2].mount_uuid.clone();
    let e = cfg.derive_recovery("7c7ae72d-09d6-4086-b249-1ac60f21b73b").unwrap_err();
    assert!(e.contains("two targets") && e.contains("primary-22tb") && e.contains("system-recovery-B-2tb"), "{e}");
}

#[test]
fn derive_recovery_output_validates_and_round_trips() {
    let cfg = test_config_with_three_targets();
    let r = cfg.derive_recovery("7c7ae72d-09d6-4086-b249-1ac60f21b73b").unwrap();
    assert!(r.validate().is_empty(), "{:?}", r.validate());
    let again = Config::from_toml(&r.to_toml().unwrap()).unwrap();
    assert!(again.general.recovery_drive);
    assert_eq!(again.targets.len(), 3);
    assert_eq!(again.targets[0].label, "system-recovery-B-2tb");
}

#[test]
fn validate_still_requires_a_source_when_not_a_recovery_drive() {
    let mut cfg = test_config_with_three_targets();
    cfg.sources.clear();
    assert!(cfg.validate().iter().any(|e| e.contains("No backup sources")));
}

fn test_config_with_three_targets() -> Config {
    let mut cfg = Config::from_toml(include_str!("../../config/config.example.toml")).unwrap();
    // Pin the three targets this suite reasons about, whatever the example carries.
    cfg.targets = vec![
        target("primary-22tb", "b2dbe07d-40b9-422e-8ccf-ef4931c40457", "/mnt/backup-22tb", TargetRole::Primary),
        target("system-recovery-A-2tb", "60b05268-7f8f-47b5-a38a-752576a1172a", "/mnt/backup-system-recovery-A", TargetRole::Mirror),
        target("system-recovery-B-2tb", "7c7ae72d-09d6-4086-b249-1ac60f21b73b", "/mnt/backup-system-recovery-B", TargetRole::Mirror),
    ];
    cfg
}

fn target(label: &str, uuid: &str, mount: &str, role: TargetRole) -> Target {
    Target { label: label.into(), mount_uuid: Some(uuid.into()), mount: mount.into(), role, ..Default::default() }
}
```

If `config/config.example.toml` does not exist under that name, use the path the existing `config.rs` tests load (grep `include_str!` / `from_toml(` in the tests module) and keep one helper. If `Target` has no `Default`, build it with every field as the existing tests do.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-backup-target cargo test --features dbus --lib -- derive_recovery validate_still_requires`
Expected: compile error, `no method named derive_recovery`.

- [ ] **Step 3: Implement**

In `config.rs`, `impl Config`:

```rust
/// The configuration a recovery drive's own OS runs with, derived from the
/// host's (spec D4): no sources; every target kept — the one whose
/// filesystem the OS booted from (by `mount_uuid`) moved first, the rest in
/// their order, so an attached 22 TB pair is still mounted by UUID for what
/// only it holds (the resolver, `Database::snapshot_path_present`, prefers
/// `targets[0]`); email, scrub and the boot step off; restores allowed only
/// under `RECOVERY_ALLOWED_ROOTS`. Refuses when no target, or more than one,
/// names `booted_uuid` — never a guess.
pub fn derive_recovery(&self, booted_uuid: &str) -> Result<Config, String> {
    let matching: Vec<usize> = self
        .targets
        .iter()
        .enumerate()
        .filter(|(_, t)| t.mount_uuid.as_deref().is_some_and(|u| u.eq_ignore_ascii_case(booted_uuid)))
        .map(|(i, _)| i)
        .collect();
    let booted = match matching.as_slice() {
        [one] => *one,
        [] => {
            let known: Vec<&str> = self.targets.iter().map(|t| t.label.as_str()).collect();
            return Err(format!(
                "no target of filesystem {booted_uuid} in this configuration (targets: {})",
                known.join(", ")
            ));
        }
        many => {
            let labels: Vec<&str> = many.iter().map(|&i| self.targets[i].label.as_str()).collect();
            return Err(format!(
                "two targets name filesystem {booted_uuid}: {} — fix the configuration, nothing is derived",
                labels.join(", ")
            ));
        }
    };
    let mut r = self.clone();
    r.general.recovery_drive = true;
    r.sources.clear();
    let own = r.targets.remove(booted);
    r.targets.insert(0, own);
    r.email.enabled = false;
    r.scrub.enabled = false;
    r.scrub.targets.clear();
    r.boot.enabled = false;
    r.restore.allowed_roots = RECOVERY_ALLOWED_ROOTS.iter().map(|s| s.to_string()).collect();
    Ok(r)
}
```

`Config` and `Target` must derive `Clone` (add if missing). In `validate` (line 763), guard the source rule:

```rust
if self.sources.is_empty() && !self.general.recovery_drive {
```

In `main.rs` `ConfigAction` add:

```rust
/// Derive a recovery drive's configuration from the host's (no sources, the
/// booted target only, email/scrub/boot off, restores under the recovery roots)
DeriveRecovery {
    /// The host's config.toml (from the newest received root snapshot)
    #[arg(long)]
    from: PathBuf,
    /// UUID of the filesystem this OS booted from (findmnt -no UUID /)
    #[arg(long)]
    booted_uuid: String,
    /// Write here instead of stdout
    #[arg(long)]
    out: Option<PathBuf>,
},
```

Handler:

```rust
ConfigAction::DeriveRecovery { from, booted_uuid, out } => {
    let cfg = Config::load(&from)?;
    let derived = match cfg.derive_recovery(&booted_uuid) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("derive-recovery: {e}");
            std::process::exit(1);
        }
    };
    match out {
        Some(p) => derived.save(&p)?,
        None => print!("{}", derived.to_toml()?),
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-backup-target cargo test --features dbus --lib -- config::`
Expected: all PASS, including the five new ones.

- [ ] **Step 5: Exercise the CLI both ways**

```bash
cmake --build /tmp/das-backup-build
B=/tmp/das-backup-build/cargo-target/release/btrdasd
sudo $B config derive-recovery --from /etc/das-backup/config.toml --booted-uuid 60b05268-7f8f-47b5-a38a-752576a1172a | grep -E '^recovery_drive|^\[\[target\]\]|^label|^allowed_roots' 
# Expected: recovery_drive = true, three [[target]] blocks with label = "system-recovery-A-2tb" FIRST, then primary-22tb, then system-recovery-B-2tb; the two roots; no [[source]].
sudo $B config derive-recovery --from /etc/das-backup/config.toml --booted-uuid 00000000-0000-0000-0000-000000000000; echo "rc=$?"
# Expected: "derive-recovery: no target of filesystem 0000…" on stderr, rc=1, nothing on stdout.
```

- [ ] **Step 6: Lint and commit**

```bash
cd indexer && cargo fmt && CARGO_TARGET_DIR=/tmp/das-backup-target cargo clippy --features dbus --all-targets -- -D warnings
cd .. && git add indexer/src/config.rs indexer/src/main.rs
git commit -m "btrdasd config derive-recovery: a recovery drive's configuration from the host's (D4)"
```

---

### Task 3: The state subvolume and the post-index send pass (host side, D8)

**Files:**

- Modify: `scripts/backup-run.sh` (new `run_state_pass()` after `run_indexer()` ~line 3002; call site in `main()` right after `run_indexer` ~line 4037; report line in `generate_report`; header `# Version:` bump)
- Modify: `docs/examples/config.example.toml` or wherever the example config lives: the `@/var/lib/das-backup` entry with `snapshot_name = "das-state"`
- Test: `tests/test_state_pass.sh` (new, ctest `shell-state-pass` in `CMakeLists.txt` beside `shell-record-run`)

**Interfaces:**

- Consumes: `record_op <op> <OK|WARN|FAIL|SKIP> [detail]`, `log_info/log_warn/log_error`, `TARGET_MOUNTS[label]`, `TARGET_ROLES[label]`, `ALL_TARGET_MOUNTS`, `DAS_BTRBK_CONF`, `BTRDASD_BIN`, `DRY_RUN` as the script names them (read the real names at the top of `backup-run.sh` and use those; do not invent).
- Produces: a btrbk snapshot series named `das-state` under the `nvme` volume's target dir on every target that receives `nvme` (`<target mount>/nvme/das-state.<stamp>`), sent after the indexer ran; `OP_STATUS[state]` with `OK`, `WARN` or `SKIP`; a report row `State index pass`.

**How the entry comes to exist (operator-run, once, with the backup lock free):**

```bash
# 1. nothing may run
systemctl stop das-backup.timer das-backup-full.timer das-scrub.timer das-backup-doctor.timer
sudo flock -n /run/das-backup.lock true && sudo flock -n /run/das-maintenance.lock true || { echo busy; exit 1; }
# 2. a nested subvolume beside the directory, filled by reflink (same filesystem: instant, no space)
sudo btrfs subvolume create /var/lib/das-backup.subvol
sudo cp -a --reflink=always /var/lib/das-backup/. /var/lib/das-backup.subvol/
# 3. swap, keeping the old directory until the next run proved the new one
sudo mv /var/lib/das-backup /var/lib/das-backup.dir-before-subvol && sudo mv /var/lib/das-backup.subvol /var/lib/das-backup
sudo btrfs subvolume show /var/lib/das-backup | head -3   # must print a subvolume, not an error
sudo sqlite3 /var/lib/das-backup/backup-index.db 'pragma quick_check;'   # ok
# 4. the entry: sync adopts nested subvolumes (backup.md); give it the clean snapshot name
sudo btrdasd subvol sync --config /etc/das-backup/config.toml
sudo grep -n -A3 'name = "@/var/lib/das-backup"' /etc/das-backup/config.toml   # the adopted entry
# add `snapshot_name = "das-state"` to that entry by hand (one line), then:
sudo btrdasd config validate && sudo btrdasd setup --upgrade   # re-renders btrbk.conf
sudo grep -n -B1 'snapshot_name       das-state' /etc/btrbk/btrbk.conf          # the rendered entry
systemctl start das-backup.timer das-backup-full.timer das-scrub.timer das-backup-doctor.timer
```

If `subvol sync` does not adopt a subvolume nested three directories deep, add the entry with `sudo btrdasd subvol add nvme @/var/lib/das-backup` and set the snapshot name the same way; record which happened in the session record. `/var/lib/das-backup.dir-before-subvol` is deleted after the first clean run that reports the state pass.

- [ ] **Step 1: Write the failing test**

`tests/test_state_pass.sh`, in the style of `tests/test_record_run.sh` (extract `run_state_pass`, `record_op`, `format_duration` if it exists, stub `btrbk`, `date`):

```bash
#!/bin/bash
# shellcheck disable=SC2034,SC2329
# tests/test_state_pass.sh — run_state_pass() of scripts/backup-run.sh against a
# stub btrbk: the second, small send of the state subvolume after indexing (D8).
set -euo pipefail
SCRIPT="$(cd "$(dirname "$0")/.." && pwd)/scripts/backup-run.sh"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
extract() { sed -n "/^$1() {/,/^}/p" "$SCRIPT"; }
for fn in run_state_pass record_op; do
    body="$(extract "$fn")"; [[ -n "$body" ]] || { echo "FAIL: $fn not found"; exit 1; }
    eval "$body"
done
passes=0; fails=0
pass() { passes=$((passes+1)); echo "ok   $1"; }
fail() { fails=$((fails+1)); echo "FAIL $1"; }
check() { if [[ "$2" == "$3" ]]; then pass "$1"; else fail "$1: got '$2', want '$3'"; fi; }
has() { if [[ "$2" == *"$3"* ]]; then pass "$1"; else fail "$1: '$3' not in: $2"; fi; }
log_info() { echo "INFO $*" >>"$WORK/log"; }
log_warn() { echo "WARN $*" >>"$WORK/log"; }
log_error() { echo "ERROR $*" >>"$WORK/log"; }
declare -A OP_STATUS TARGET_MOUNTS TARGET_ROLES
DAS_BTRBK_CONF="$WORK/btrbk.conf"; : >"$DAS_BTRBK_CONF"
DRY_RUN=false
mkdir -p "$WORK/bin"; export PATH="$WORK/bin:$PATH"
cat >"$WORK/bin/btrbk" <<'EOF'
#!/bin/bash
echo "$*" >>"$STUB/btrbk.calls"
[[ -f "$STUB/btrbk.fails" ]] && { echo "ERROR: send failed" >&2; exit 1; }
echo "sent 12345678 bytes"
EOF
chmod +x "$WORK/bin/btrbk"; export STUB="$WORK"
fixture() { rm -f "$WORK/btrbk.calls" "$WORK/btrbk.fails" "$WORK/log"; OP_STATUS=(); TARGET_MOUNTS=([primary-22tb]=/mnt/backup-22tb [system-recovery-A-2tb]=/mnt/backup-system-recovery-A [system-recovery-B-2tb]=/mnt/backup-system-recovery-B); TARGET_ROLES=([primary-22tb]=primary [system-recovery-A-2tb]=mirror [system-recovery-B-2tb]=mirror); MOUNTED=(/mnt/backup-22tb /mnt/backup-system-recovery-A /mnt/backup-system-recovery-B); }
mountpoint() { local p="${*: -1}"; for m in "${MOUNTED[@]}"; do [[ "$m" == "$p" ]] && return 0; done; return 1; }

echo "--- every mounted target gets the filter, one btrbk call"
fixture; run_state_pass
check "state pass: OK" "${OP_STATUS[state]}" "OK"
check "state pass: one call" "$(wc -l <"$WORK/btrbk.calls")" "1"
has "state pass: primary filter" "$(cat "$WORK/btrbk.calls")" "/mnt/backup-22tb/nvme/das-state"
has "state pass: A filter" "$(cat "$WORK/btrbk.calls")" "/mnt/backup-system-recovery-A/nvme/das-state"
has "state pass: B filter" "$(cat "$WORK/btrbk.calls")" "/mnt/backup-system-recovery-B/nvme/das-state"
has "state pass: run, not dryrun" "$(cat "$WORK/btrbk.calls")" " run "

echo "--- an absent target is left out, the pass still runs (review focus 3)"
fixture; MOUNTED=(/mnt/backup-22tb /mnt/backup-system-recovery-B); run_state_pass
check "absent target: OK" "${OP_STATUS[state]}" "OK"
has "absent target: B still sent" "$(cat "$WORK/btrbk.calls")" "/mnt/backup-system-recovery-B/nvme/das-state"
if [[ "$(cat "$WORK/btrbk.calls")" == *"/mnt/backup-system-recovery-A/"* ]]; then fail "absent target: A was passed to btrbk"; else pass "absent target: A not passed"; fi

echo "--- no target mounted: skipped, said"
fixture; MOUNTED=(); run_state_pass
check "nothing mounted: SKIP" "${OP_STATUS[state]}" "SKIP"
has "nothing mounted: why" "${OP_STATUS[state_detail]}" "no target mounted"
check "nothing mounted: btrbk never called" "$(cat "$WORK/btrbk.calls" 2>/dev/null | wc -l)" "0"

echo "--- btrbk fails: WARN with its words, never OK"
fixture; touch "$WORK/btrbk.fails"; run_state_pass
check "btrbk fails: WARN" "${OP_STATUS[state]}" "WARN"
has "btrbk fails: says why" "${OP_STATUS[state_detail]}" "send failed"
has "btrbk fails: logged" "$(cat "$WORK/log")" "WARN"

echo "--- dry run: nothing sent, said"
fixture; DRY_RUN=true; run_state_pass; DRY_RUN=false
check "dry run: SKIP" "${OP_STATUS[state]}" "SKIP"
check "dry run: btrbk never called" "$(cat "$WORK/btrbk.calls" 2>/dev/null | wc -l)" "0"

echo; echo "$passes passed, $fails failed"
((fails == 0)) || exit 1
echo "STATE-PASS SUITE GREEN"
```

Replace `DRY_RUN`, `TARGET_MOUNTS`, `TARGET_ROLES` with the script's real variable names if they differ (check the top of `backup-run.sh` and `run_indexer`). The volume subdir `nvme` is the source label of the `@` volume; read it from the config the way `run_indexer` reads the primary mount if the script already has the source→subdir map, otherwise derive it from `btrbk.conf`'s `target` lines for the `das-state` entry (`grep -B20 'snapshot_name       das-state' "$DAS_BTRBK_CONF" | grep '^  target '`), which is the authoritative rendered layout and needs no new map.

- [ ] **Step 2: Run the test to verify it fails**

Run: `bash tests/test_state_pass.sh`
Expected: `FAIL: run_state_pass not found`, exit 1.

- [ ] **Step 3: Implement `run_state_pass`**

In `scripts/backup-run.sh` after `run_indexer()`:

```bash
# The second, small send after indexing (spec D8): the state subvolume
# (/var/lib/das-backup — the index, the records, the history) goes out AFTER
# the indexer updated it, so every target's copy of the index matches the
# data it received tonight. One btrbk call, one filter per mounted target
# (filters are a union), `run` not `dryrun`. An absent target is left out,
# not failed; a failed send is a WARN with btrbk's words — the data went
# out already, only the index copy is a night old — never recorded as OK.
run_state_pass() {
    if [[ "$DRY_RUN" == true ]]; then
        record_op "state" "SKIP" "dry run"
        return
    fi
    local filters=() label mnt
    for label in "${!TARGET_MOUNTS[@]}"; do
        mnt="${TARGET_MOUNTS[$label]}"
        mountpoint -q "$mnt" 2>/dev/null || continue
        filters+=("$mnt/nvme/das-state")
    done
    if ((${#filters[@]} == 0)); then
        record_op "state" "SKIP" "no target mounted"
        return
    fi
    log_info "Sending the state subvolume after indexing (${#filters[@]} target(s))..."
    local start=$SECONDS out
    if out=$(btrbk -c "$DAS_BTRBK_CONF" run "${filters[@]}" 2>&1); then
        record_op "state" "OK" "$((SECONDS - start))s, ${#filters[@]} target(s)"
        log_info "state subvolume sent: $((SECONDS - start))s"
    else
        local why
        why=$(printf '%s' "$out" | grep -m1 -iE 'error|fail' || printf '%s' "$out" | tail -n1)
        record_op "state" "WARN" "state send failed: ${why:-btrbk exit $?}"
        log_warn "state subvolume send failed: ${why:-no output} -- the targets' index copy is a night old"
    fi
}
```

Use the script's real variable names (`DRY_RUN`, `TARGET_MOUNTS`) — adjust to what Step 1 settled. Call it in `main()` right after `run_indexer` (both in the branch that runs the indexer):

```bash
        run_indexer
        run_state_pass
```

Add the report row where `generate_report` prints `Content indexer`: `State index pass` with `${OP_STATUS[state]}` and its detail, in the same format as the row above it. A `WARN` here must make the run status `COMPLETED WITH WARNINGS` the way other WARN ops do (check `any_op_is`/`run_status` — if they already fold any `WARN` op, nothing to add).

- [ ] **Step 4: Run the test to verify it passes**

Run: `bash tests/test_state_pass.sh`
Expected: every line `ok`, `STATE-PASS SUITE GREEN`.

- [ ] **Step 5: The whole script's suites, shellcheck, ctest registration**

```bash
shellcheck -S warning scripts/backup-run.sh tests/test_state_pass.sh
bash tests/test_record_run.sh | tail -1        # still GREEN
bash tests/test_backup_exit_semantics.sh | tail -1
```

Add to `CMakeLists.txt` beside `shell-record-run`:

```cmake
    add_test(
        NAME shell-state-pass
        COMMAND bash ${CMAKE_CURRENT_SOURCE_DIR}/tests/test_state_pass.sh
    )
```

Bump `# Version:` in `backup-run.sh` (minor) and its `# Date:`.

- [ ] **Step 6: Commit**

```bash
git add scripts/backup-run.sh tests/test_state_pass.sh CMakeLists.txt
git commit -m "backup-run: send the state subvolume after indexing, so every target's index matches its data (D8)"
```

- [ ] **Step 7: The operator-run migration above, then one real run, then the proof**

After the host migration block above, run `sudo systemctl start das-backup.service` (or wait for the night), then:

```bash
journalctl -u das-backup --since today -o cat | grep -E 'state subvolume|State index pass'
sudo mount -o subvolid=5,ro UUID=60b05268-7f8f-47b5-a38a-752576a1172a /mnt/tmp-A 2>/dev/null || echo "A not attached — check the 22 TB instead"
# On whichever target is mounted by the run: the newest das-state snapshot's DB must be NEWER than the newest root- snapshot's copy
```

The proof: `ls -t <target>/nvme/ | head` shows `das-state.<stamp>` with the same stamp as tonight's `root-.<stamp>`, and `stat` of `backup-index.db` inside it is newer than the one inside `root-.<stamp>/var/lib/das-backup/` (which, now that it is a nested subvolume, is an empty directory — that emptiness is itself the proof that the state moved).

Then `sudo rm -rf /var/lib/das-backup.dir-before-subvol` and note it in the session record.

---

### Task 4: `das-recovery-state-sync.sh` and its unit

**Files:**

- Create: `scripts/das-recovery-state-sync.sh`
- Create: `systemd/das-recovery-state-sync.service`
- Modify: `CMakeLists.txt` (install the script to `lib/das-backup/`, the unit to `lib/systemd/system`, beside `btrdasd-helper.service` ~line 188; ctest `shell-recovery-state-sync`)
- Test: `tests/test_recovery_state_sync.sh` (new)

**Interfaces:**

- Consumes: `btrdasd config derive-recovery --from F --booted-uuid U --out O` (Task 2); `btrdasd walk <mount> --db <db> --config <cfg>` (existing); `findmnt`, `mount`, `umount`, `cp --reflink`, `btrfs subvolume list`; `Config.general.recovery_drive` (Task 1).
- Produces: at boot on a recovery drive, `/var/lib/das-backup/` populated from the newest received `das-state.<stamp>`; `/etc/das-backup/config.toml` derived from the newest received `root-.<stamp>`'s `etc/das-backup/config.toml`; a status file `/run/das-recovery-state/status` with one of `synced <stamp>`, `synced <stamp>; catch-up index running`, `no state snapshot`, `failed: <why>`; exit 0 on sync, 1 otherwise. Plan 2's provisioning enables the unit and writes the marker `/etc/das-backup/recovery-drive`.

Design of the script (all paths and tools overridable by `DAS_STATE_SYNC_*` env vars for the test, the same seam style as `recovery-os-vm.sh`'s `TEST_ROOT`):

```bash
#!/bin/bash
# das-recovery-state-sync.sh — on a recovery drive's own OS, at boot: the
# newest received copy of btrdasd's state (index, records, history) reflinked
# into /var/lib/das-backup, and the host's config.toml, from the newest
# received root snapshot, derived into this OS's /etc/das-backup/config.toml
# (btrdasd config derive-recovery). Same btrfs: the copy is instant and costs
# no space. Then, only when the state is older than the data (a night the
# state pass failed), an incremental index catch-up in the background.
# Version: 1.0.0
# Date: <set on creation>
#
# Never runs on the host: ConditionPathExists=/etc/das-backup/recovery-drive
# in the unit, and the marker is written only by provisioning. A failure
# leaves whatever /var/lib/das-backup held — never an empty database — and
# exits 1 with the reason in the journal and in $STATUS_FILE.
set -euo pipefail
ROOT="${DAS_STATE_SYNC_ROOT:-}"                       # test seam: a fake filesystem tree
STATE_DIR="${DAS_STATE_SYNC_STATE_DIR:-$ROOT/var/lib/das-backup}"
CONFIG_OUT="${DAS_STATE_SYNC_CONFIG_OUT:-$ROOT/etc/das-backup/config.toml}"
MARKER="${DAS_STATE_SYNC_MARKER:-$ROOT/etc/das-backup/recovery-drive}"
RUN_DIR="${DAS_STATE_SYNC_RUN_DIR:-$ROOT/run/das-recovery-state}"
TOP="${DAS_STATE_SYNC_TOP:-$RUN_DIR/top}"             # the booted fs mounted at subvolid=5
BTRDASD="${DAS_STATE_SYNC_BTRDASD:-/usr/bin/btrdasd}"
STATUS_FILE="$RUN_DIR/status"

say() { printf '%s\n' "$*"; }
status() { mkdir -p "$RUN_DIR"; printf '%s\n' "$*" >"$STATUS_FILE"; }
die() { say "das-recovery-state-sync: $*" >&2; status "failed: $*"; exit 1; }

[[ -e "$MARKER" ]] || die "not a recovery drive ($MARKER absent): nothing done"
uuid=$(findmnt -no UUID "${ROOT:-/}") || die "cannot read the booted filesystem's UUID"
[[ -n "$uuid" ]] || die "the booted filesystem has no UUID"
src=$(findmnt -no SOURCE "${ROOT:-/}"); src=${src%%[*}
mkdir -p "$TOP"
if [[ -z "$ROOT" ]]; then
    mount -o subvolid=5,ro "$src" "$TOP" || die "cannot mount the booted filesystem's top level read-only"
    trap 'umount "$TOP" 2>/dev/null || :' EXIT
fi
# The drive's own received directory for the nvme volume: <mount>/nvme is how
# the host lays it out (btrbk.conf target lines); on the drive it is the
# top-level directory named like the target's subdir.
dir="$TOP/nvme"
[[ -d "$dir" ]] || die "no received 'nvme' directory on this filesystem: not a target that receives the host's root"
newest() { ls -1d "$dir/$1".* 2>/dev/null | sort | tail -n1; }
state_snap=$(newest das-state); root_snap=$(newest root-)
[[ -n "$root_snap" ]] || die "no received root- snapshot: the host's config.toml cannot be derived"
[[ -r "$root_snap/etc/das-backup/config.toml" ]] || die "the newest root snapshot ${root_snap##*/} has no etc/das-backup/config.toml"
[[ -n "$state_snap" ]] || { status "no state snapshot"; die "no received das-state snapshot yet (the next backup run sends one): state left as it was"; }
# config first — derive-recovery refuses before anything is written when the UUID is unknown
mkdir -p "$(dirname "$CONFIG_OUT")" "$STATE_DIR.new"
"$BTRDASD" config derive-recovery --from "$root_snap/etc/das-backup/config.toml" --booted-uuid "$uuid" --out "$CONFIG_OUT.new" || die "derive-recovery refused (see above): state left as it was"
# state: reflink into a sibling, then swap — the old copy is never half-replaced
rm -rf "$STATE_DIR.new"; mkdir -p "$STATE_DIR.new"
cp -a --reflink=auto "$state_snap/." "$STATE_DIR.new/" || { rm -rf "$STATE_DIR.new"; die "copying ${state_snap##*/} failed: state left as it was"; }
if [[ -d "$STATE_DIR" ]]; then rm -rf "$STATE_DIR.old"; mv "$STATE_DIR" "$STATE_DIR.old"; fi
mv "$STATE_DIR.new" "$STATE_DIR"; rm -rf "$STATE_DIR.old"
mv "$CONFIG_OUT.new" "$CONFIG_OUT"
stamp=${state_snap##*.}; data_stamp=${root_snap##*.}
if [[ "$stamp" < "$data_stamp" ]]; then
    # the state is older than the data (review focus 1): catch up in the background
    status "synced $stamp; catch-up index running (data $data_stamp)"
    say "state $stamp is older than the newest data $data_stamp: starting an incremental index catch-up"
    systemd-run --quiet --collect --unit=das-recovery-state-catchup --property=Type=oneshot \
        "$BTRDASD" walk "$TOP/nvme" --db "$STATE_DIR/backup-index.db" --config "$CONFIG_OUT" \
        || say "could not start the catch-up (systemd-run failed): search is current to $stamp only" >&2
else
    status "synced $stamp"
fi
say "state synced from ${state_snap##*/}; configuration derived from ${root_snap##*/}"
```

(The `walk` target path on a recovery OS is the drive's own target mount from the derived config, `/mnt/backup-system-recovery-<X>`, which `walk --config` mounts by `mount_uuid`; if `walk` cannot mount the booted filesystem a second time, pass `$TOP/nvme`'s parent as above and verify in Step 5 which one `walk` accepts. The test only checks that walk is invoked with the DB and config paths.)

Unit `systemd/das-recovery-state-sync.service`:

```ini
[Unit]
Description=Recovery drive: sync btrdasd state and configuration from the newest received snapshot
ConditionPathExists=/etc/das-backup/recovery-drive
After=local-fs.target
Before=display-manager.service btrdasd-helper.service
DefaultDependencies=no

[Service]
Type=oneshot
ExecStart=/usr/lib/das-backup/das-recovery-state-sync.sh
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
```

- [ ] **Step 1: Write the failing test**

`tests/test_recovery_state_sync.sh`: a fixture tree under `$WORK/root` with `etc/das-backup/recovery-drive`, `run/`, `var/lib/das-backup/` holding a sentinel file `previous`, and `nvme/` holding `root-.20261012T0348/etc/das-backup/config.toml` (any text) and `das-state.20261012T0348/backup-index.db` (text "db-0348"); stub `findmnt` (prints a UUID / a source), stub `btrdasd` that logs its args and for `config derive-recovery` writes `recovery_drive = true` to `--out`, stub `systemd-run` that logs. Cases, each a `fixture` then a run with `DAS_STATE_SYNC_ROOT=$WORK/root DAS_STATE_SYNC_TOP=$WORK/root`:

```text
synced: exit 0; var/lib/das-backup/backup-index.db == db-0348; previous sentinel gone; config.toml written with recovery_drive = true; status "synced 20261012T0348"; derive-recovery called with --booted-uuid <stub uuid> and --from .../root-.20261012T0348/etc/das-backup/config.toml; systemd-run NOT called
gap (review focus 1): add root-.20261013T0348 only; exit 0; status "synced 20261012T0348; catch-up index running (data 20261013T0348)"; systemd-run called with "walk" and "--db .../backup-index.db"
no state snapshot (review focus 2): remove das-state.*; exit 1; previous sentinel STILL present; status "no state snapshot"; config.toml NOT written
no marker: remove etc/das-backup/recovery-drive; exit 1; nothing written; message "not a recovery drive"
derive refuses: stub btrdasd exits 1 on derive-recovery; exit 1; previous sentinel still present; config.toml not written; status "failed: derive-recovery refused"
copy fails: make das-state.* unreadable (chmod 000, skipped when root); exit 1; previous sentinel still present
```

Write it with the same `check`/`has`/`pass`/`fail` helpers as Task 3's test, ending with `RECOVERY-STATE-SYNC SUITE GREEN`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `bash tests/test_recovery_state_sync.sh`
Expected: the script is absent — `FAIL` on the first case (no such file), exit 1.

- [ ] **Step 3: Write the script and the unit as designed above**

Set `# Date:` from `date +%F`. `chmod +x scripts/das-recovery-state-sync.sh`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `bash tests/test_recovery_state_sync.sh`
Expected: every case `ok`, `RECOVERY-STATE-SYNC SUITE GREEN`.

- [ ] **Step 5: Install rules, shellcheck, a real dry exercise on the host (must refuse)**

`CMakeLists.txt`: add `scripts/das-recovery-state-sync.sh` to the `lib/das-backup` install list (beside `recovery-os-vm.sh`, ~line 144) and `systemd/das-recovery-state-sync.service` beside `btrdasd-helper.service` (~line 188); add ctest `shell-recovery-state-sync`. Then:

```bash
shellcheck -S warning scripts/das-recovery-state-sync.sh
cmake --build /tmp/das-backup-build && sudo cmake --install /tmp/das-backup-build >/dev/null
sudo /usr/lib/das-backup/das-recovery-state-sync.sh; echo "rc=$?"
# Expected on the host: "not a recovery drive (/etc/das-backup/recovery-drive absent): nothing done", rc=1, /var/lib/das-backup untouched (ls -la before/after identical)
systemctl cat das-recovery-state-sync.service | head -3   # installed, not enabled
systemctl is-enabled das-recovery-state-sync.service      # disabled
```

The host refusal is the unit's own falsification: the marker gate is seen refusing on a machine where running would be wrong.

- [ ] **Step 6: Commit**

```bash
git add scripts/das-recovery-state-sync.sh systemd/das-recovery-state-sync.service tests/test_recovery_state_sync.sh CMakeLists.txt
git commit -m "das-recovery-state-sync: a recovery drive's state and configuration from its newest received snapshot at boot (D4, D8)"
```

---

### Task 5: Packaging, docs, changelog

**Files:**

- Modify: `packaging/arch/PKGBUILD` (nothing if it installs via cmake; verify the new files land: `makepkg`-free check `grep -n cmake packaging/arch/PKGBUILD`), `packaging/debian/rules`, `packaging/fedora/*.spec`, `packaging/snap`, `packaging/flatpak` — each format lists or globs installed files; add the two new files where a list exists.
- Modify: `docs/DISASTER-RECOVERY-GUIDE.md` — new subsection under the recovery-drive chapter: "What the recovery OS knows at boot" (state sync, derived configuration, the two restore roots, the catch-up, the status file, what `no state snapshot` means); the restore chapter: "Restoring from a recovery drive's own copy" (the GUI resolves the drive's copy; `btrdasd restore file <path under /mnt/backup-system-recovery-X/...>`).
- Modify: `docs/btrdasd.1` — `config derive-recovery`.
- Modify: `CHANGELOG.md` Unreleased — Added: `config derive-recovery`, the state pass, the sync unit, the resolver; Changed: `/var/lib/das-backup` is a nested subvolume on the author's host (migration steps in the guide).
- Modify: `.claude/rules/backup.md` — one bullet under the content indexer: the state subvolume is sent after indexing; a WARN from the state pass means the targets' index is a night old.

- [ ] **Step 1: Make the edits**

Write the guide text from the script's and subcommand's actual messages (quote them verbatim; a reader greps the guide for the words they saw).

- [ ] **Step 2: Lint**

```bash
markdownlint docs/DISASTER-RECOVERY-GUIDE.md CHANGELOG.md .claude/rules/backup.md | grep -v MD013
codespell docs/DISASTER-RECOVERY-GUIDE.md CHANGELOG.md scripts/das-recovery-state-sync.sh
man -l docs/btrdasd.1 >/dev/null
```

- [ ] **Step 3: Full verification before the final commit**

```bash
cd indexer && CARGO_TARGET_DIR=/tmp/das-backup-target cargo test --features dbus 2>&1 | grep -E 'test result|FAILED'
cd .. && cmake --build /tmp/das-backup-build && (cd /tmp/das-backup-build && ctest --output-on-failure -R '^shell-')
```

Expected: every Rust suite `ok`, every shell suite `Passed`, including the three new ones.

- [ ] **Step 4: Commit and push; CI must be green, including Mutants**

```bash
git add -A packaging docs CHANGELOG.md .claude/rules/backup.md
git commit -m "docs, packaging: recovery state sync, derive-recovery, the state pass"
~/.claude/bin/scrub-promo check origin/main..HEAD && git push origin main
gh run list --limit 4   # wait for all four: success
```

If Mutants reports survivors in `config.rs` or `db.rs`, the surviving mutant names the missing assertion; add it to the owning test (Task 1 or 2), do not weaken the gate.

---

## Self-review

- **Spec coverage:** D8 mechanism (Task 3), D8 fallback (Task 4 gap branch), D4 option C (Task 2 + Task 4), state on the drive at boot (Task 4), GUI/helper restore from the drive's own copy (Task 1), docs and packaging (Task 5). The marker file and enabling the unit belong to plan 2 (provisioning), stated in Task 4's interfaces.
- **Placeholder scan:** none; every step has its code or its exact command.
- **Type consistency:** `snapshot_path_present(id, prefer_root: Option<&str>, exists)` used identically in Task 1's tests and helper; `derive_recovery(&self, booted_uuid: &str) -> Result<Config, String>` in Task 2 and called by the CLI; the CLI flags `--from/--booted-uuid/--out` match Task 4's invocation; `OP_STATUS[state]`/`[state_detail]` match `record_op`'s convention.
- **Review focus:** 1 → Task 4 gap test; 2 → Task 4 no-snapshot test; 3 → Task 3 absent-target test; 4 → Task 1 prefer-own-target test; 5 → Task 2 ambiguous-uuid test.
