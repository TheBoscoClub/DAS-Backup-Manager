//! Keep `config.toml` in step with the subvolumes that actually exist.
//!
//! The backup used to be an allowlist: a subvolume was backed up only if an
//! entry named it, and `btrfs send` does not descend into nested subvolumes,
//! so a missing entry meant a silently empty directory in every snapshot.
//! This module inverts that. See
//! `docs/superpowers/specs/2026-10-01-subvolume-reconcile-design.md`.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use crate::btrbk_conf::{algorithmic_snapshot_name, resolve_snapshot_names};
use crate::config::{Config, Source, SubvolConfig, TargetRole};
use crate::doctor::glob_match;
use crate::fsutil::{CommandRunner, write_atomic};

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
/// the first ` path `, because a path may itself contain spaces. Only lines
/// that start with `ID ` count — an error or warning that happens to contain
/// ` path ` must not become a phantom subvolume — and a line with nothing
/// after ` path ` is ignored.
pub fn parse_subvolume_paths(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|line| line.starts_with("ID "))
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    SnapshotTree,
    Excluded { pattern: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    pub volume: String,
    pub name: String,
    pub source_label: String,
    pub nested_under: Option<String>,
    pub manual_only: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRef {
    pub source_label: String,
    pub name: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub volume: String,
    pub name: String,
    pub reason: SkipReason,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unplaceable {
    pub volume: String,
    pub name: String,
    pub why: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub adopt: Vec<Adoption>,
    pub retire: Vec<EntryRef>,
    pub revive: Vec<EntryRef>,
    pub skipped: Vec<Skip>,
    pub unplaceable: Vec<Unplaceable>,
    pub failed_volumes: Vec<(String, String)>,
}

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
            Some(VolumeListing {
                subvolumes: Ok(found),
                ..
            }) => found,
            Some(VolumeListing {
                subvolumes: Err(why),
                ..
            }) => {
                plan.failed_volumes.push((volume.to_string(), why.clone()));
                continue;
            }
            None => {
                plan.failed_volumes
                    .push((volume.to_string(), "volume was not listed".to_string()));
                continue;
            }
        };
        let sources: Vec<&Source> = config
            .sources
            .iter()
            .filter(|s| s.volume == volume)
            .collect();

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
                    reason: SkipReason::Excluded {
                        pattern: pattern.to_string(),
                    },
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
                    // `sources` is non-empty: the volume came from a source. When
                    // every source here is itself an adoption source, that one is
                    // the home; appending `-adopted` again would invent a second.
                    source_label: adoption_source_label(config, volume)
                        .unwrap_or_else(|| sources[0].label.clone()),
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
    // Bounded so a broken predicate fails fast instead of looping forever;
    // 65 534 collisions on one base name cannot occur in a real config.
    (2..=u16::MAX)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("more than 65 000 snapshot names share one base")
}

/// The config that results from carrying out `plan` on `today`. A plan item
/// that cannot be carried out — it names an entry or a volume the config does
/// not have — is an error and no config is returned: a half-applied plan would
/// look complete while leaving a subvolume unprotected.
pub fn apply_plan(config: &Config, plan: &SyncPlan, today: &str) -> Result<Config, String> {
    let mut out = config.clone();

    for (verb, items, retired) in [
        ("retire", &plan.retire, Some(today.to_string())),
        ("revive", &plan.revive, None),
    ] {
        for reference in items {
            let entry = entry_mut(&mut out, reference).ok_or_else(|| {
                format!(
                    "cannot {verb} '{}' in source '{}': no such entry",
                    reference.name, reference.source_label
                )
            })?;
            entry.retired = retired.clone();
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
        let index = match out
            .sources
            .iter()
            .position(|s| s.label == adoption.source_label)
        {
            Some(index) => index,
            None => {
                let model = config
                    .sources
                    .iter()
                    .find(|s| s.volume == adoption.volume)
                    .ok_or_else(|| {
                        format!(
                            "cannot adopt '{}': no source is configured for volume '{}'",
                            adoption.name, adoption.volume
                        )
                    })?;
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
                out.sources.len() - 1
            }
        };
        let snapshot_name = unique_name(&algorithmic_snapshot_name(&adoption.name), &taken);
        taken.insert(snapshot_name.clone());
        out.sources[index].subvolumes.push(SubvolConfig {
            name: adoption.name.clone(),
            manual_only: adoption.manual_only,
            snapshot_name: Some(snapshot_name),
            adopted: Some(today.to_string()),
            retired: None,
        });
    }
    Ok(out)
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

fn run_stdout(runner: &dyn CommandRunner, program: &str, args: &[&str]) -> Result<String, String> {
    let what = format!("{program} {}", args.join(" "));
    match runner.output(Command::new(program).args(args)) {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => Err(format!(
            "'{what}' failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
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
        let found = run_stdout(
            runner,
            "findmnt",
            &["-n", "-o", "UUID,FSROOT", "--target", volume],
        )?;
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

/// One listing per distinct volume among the config's sources, in the order
/// each first appears. Sources may share a volume while naming different
/// devices, so EVERY distinct device named for a volume must verify; one that
/// does not makes the whole volume unreadable, and the message names the
/// source and device so the operator can see which entry is wrong.
pub fn list_volumes(
    config: &Config,
    runner: &dyn CommandRunner,
    is_mountpoint: &dyn Fn(&Path) -> bool,
) -> Vec<VolumeListing> {
    let mut listings: Vec<VolumeListing> = Vec::new();
    let mut checked: Vec<(&str, &str)> = Vec::new();
    for source in &config.sources {
        let key = (source.volume.as_str(), source.device.as_str());
        if checked.contains(&key) {
            continue;
        }
        checked.push(key);
        let mut listing = list_volume(runner, is_mountpoint, &source.volume, &source.device);
        if let Err(why) = &listing.subvolumes {
            listing.subvolumes = Err(format!(
                "source '{}' names device '{}': {why}",
                source.label, source.device
            ));
        }
        match listings.iter_mut().find(|l| l.volume == source.volume) {
            None => listings.push(listing),
            // The first failure is the one reported; a later success never
            // clears it.
            Some(existing) if existing.subvolumes.is_ok() => *existing = listing,
            Some(_) => {}
        }
    }
    listings
}

/// Whether `btrbk.conf` said what `config.toml` renders to. Checked on every
/// sync whose plan leaves `config.toml` alone: an entry can reach
/// `config.toml` without passing through sync (the GUI helper, a hand edit),
/// and until `btrbk.conf` names it, it is not backed up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BtrbkConf {
    /// Matched, or the plan rewrote both files anyway.
    #[default]
    Current,
    /// Did not match and was replaced.
    Regenerated,
    /// Does not match; a real run would replace it.
    WouldRegenerate,
    /// Does not match and was left as it is: the sync failed (a volume was
    /// not read), or the replacement could not be written (`write_error`).
    OutOfDate,
}

/// What a sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    pub plan: SyncPlan,
    /// Whether `config.toml` and `btrbk.conf` were replaced.
    pub written: bool,
    /// Why they were not, when the plan called for it.
    pub write_error: Option<String>,
    pub btrbk_conf: BtrbkConf,
    /// Why the retirements in the plan were not stamped: the system clock
    /// cannot be trusted (`caldate::untrusted_clock`). Adoption and revival
    /// do not depend on the date and go ahead.
    pub retire_refused: Option<String>,
}

impl SyncOutcome {
    pub fn failed(&self) -> bool {
        self.plan.failed() || self.write_error.is_some() || self.retire_refused.is_some()
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

    let listings = list_volumes(&config, runner, is_mountpoint);

    let plan = plan_sync(&config, &listings);
    // A retirement date starts the expiry clock, so a wrong one could delete
    // backups early. With an untrusted clock the retirements are held back
    // (and reported); the rest of the plan does not depend on the date.
    let retire_refused = if plan.retire.is_empty() {
        None
    } else {
        crate::caldate::untrusted_clock(today)
    };
    let to_apply = if retire_refused.is_some() {
        SyncPlan {
            retire: Vec::new(),
            ..plan.clone()
        }
    } else {
        plan.clone()
    };
    if !to_apply.changes_config() {
        let (btrbk_conf, write_error) =
            bring_btrbk_conf_into_line(&config, dry_run, to_apply.failed());
        return Ok(SyncOutcome {
            plan,
            written: false,
            write_error,
            btrbk_conf,
            retire_refused,
        });
    }
    if dry_run {
        return Ok(SyncOutcome {
            plan,
            retire_refused,
            ..Default::default()
        });
    }

    let write_error = write_updated(&config, apply_plan(&config, &to_apply, today), config_path);
    Ok(SyncOutcome {
        written: write_error.is_none(),
        plan,
        write_error,
        btrbk_conf: BtrbkConf::Current,
        retire_refused,
    })
}

/// Make `btrbk.conf` say what `config.toml` renders to, when nothing else in
/// this sync rewrites it. Anything but an exact match — a different text, a
/// missing file, an unreadable one — counts as out of date. Nothing is written
/// on a dry run, nor while the sync has failed (a volume was not read): the
/// file is then left alone and reported out of date. A plan that itself
/// changes the config never comes here; it writes both files regardless. Returns the state and, when the
/// replacement could not be made, why.
fn bring_btrbk_conf_into_line(
    config: &Config,
    dry_run: bool,
    sync_failed: bool,
) -> (BtrbkConf, Option<String>) {
    let conf_path = Path::new(&config.general.btrbk_conf);
    let rendered = crate::btrbk_conf::render_btrbk_conf(config);
    if std::fs::read_to_string(conf_path).is_ok_and(|text| text == rendered) {
        return (BtrbkConf::Current, None);
    }
    if sync_failed {
        return (BtrbkConf::OutOfDate, None);
    }
    if dry_run {
        return (BtrbkConf::WouldRegenerate, None);
    }
    let errors = config.validate();
    if !errors.is_empty() {
        return (
            BtrbkConf::OutOfDate,
            Some(format!(
                "config.toml is not valid, so {} was not regenerated from it: {}",
                conf_path.display(),
                errors.join("; ")
            )),
        );
    }
    match write_atomic(conf_path, &rendered) {
        Ok(()) => (BtrbkConf::Regenerated, None),
        Err(e) => (
            BtrbkConf::OutOfDate,
            Some(format!("could not write {}: {e}", conf_path.display())),
        ),
    }
}

/// Replace `btrbk.conf` and `config.toml` together or not at all. Returns the
/// reason when nothing was written (or, if the rollback also failed, when the
/// two files may disagree), `None` on success. If saving `config.toml` fails
/// after `btrbk.conf` was replaced, the rollback re-renders `btrbk.conf` from
/// the old config; it does not restore the original bytes. A plan that could not be
/// applied (`Err`) is refused like a failed write.
fn write_updated(
    config: &Config,
    updated: Result<Config, String>,
    config_path: &Path,
) -> Option<String> {
    let updated = match updated {
        Ok(updated) => updated,
        Err(why) => return Some(why),
    };
    let errors = updated.validate();
    if !errors.is_empty() {
        return Some(format!(
            "the updated config is not valid: {}",
            errors.join("; ")
        ));
    }
    // btrbk.conf first: if it cannot be written, config.toml is left
    // describing what btrbk will actually do.
    let conf_path = Path::new(&updated.general.btrbk_conf);
    if let Err(e) = write_atomic(conf_path, &crate::btrbk_conf::render_btrbk_conf(&updated)) {
        return Some(format!("could not write {}: {e}", conf_path.display()));
    }
    if let Err(e) = updated.save(config_path) {
        // Put btrbk.conf back so the two files still agree.
        let restore = write_atomic(conf_path, &crate::btrbk_conf::render_btrbk_conf(config));
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
}

/// The "SUBVOLUME SYNC" section of the run report.
pub fn format_sync_report(outcome: &SyncOutcome, dry_run: bool) -> String {
    let plan = &outcome.plan;
    let mut r = String::from("SUBVOLUME SYNC\n");
    if !plan.changes_config()
        && !plan.failed()
        && plan.skipped.is_empty()
        && outcome.write_error.is_none()
        && outcome.btrbk_conf == BtrbkConf::Current
    {
        r.push_str("  No new, vanished or returning subvolumes.\n");
        return r;
    }
    if let Some(why) = &outcome.write_error {
        r.push_str(&format!("  CONFIG NOT UPDATED: {why}\n"));
    }
    match outcome.btrbk_conf {
        BtrbkConf::Current => {}
        BtrbkConf::Regenerated => r.push_str(
            "  btrbk.conf was out of date with config.toml and has been regenerated.\n",
        ),
        BtrbkConf::WouldRegenerate => r.push_str(
            "  btrbk.conf is out of date with config.toml and would be regenerated (dry run, nothing written).\n",
        ),
        BtrbkConf::OutOfDate => r.push_str(
            "  btrbk.conf is out of date with config.toml and was NOT regenerated.\n",
        ),
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
        r.push_str(&heading(
            "Adopted (now backed up)",
            "Would adopt",
            "NOT adopted",
        ));
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
        match &outcome.retire_refused {
            Some(why) => r.push_str(&format!("  NOT retired ({why}):\n")),
            None => r.push_str(&heading(
                "Retired (gone from disk; existing backups will expire)",
                "Would retire",
                "NOT retired",
            )),
        }
        for e in &plan.retire {
            r.push_str(&format!("    {}  [source {}]\n", e.name, e.source_label));
        }
    }
    if !plan.revive.is_empty() {
        r.push_str(&heading(
            "Revived (back on disk)",
            "Would revive",
            "NOT revived",
        ));
        for e in &plan.revive {
            r.push_str(&format!("    {}  [source {}]\n", e.name, e.source_label));
        }
    }
    if !plan.skipped.is_empty() {
        r.push_str("  Skipped:\n");
        // An exclusion is a decision someone made, so each one is named with
        // its pattern. Snapshot trees hold one subvolume per retained
        // snapshot, so naming each would bury the lines that matter; they
        // are counted per volume instead.
        let mut trees: Vec<(&str, usize)> = Vec::new();
        for s in &plan.skipped {
            match &s.reason {
                SkipReason::Excluded { pattern } => r.push_str(&format!(
                    "    {}  [{}, excluded by '{pattern}']\n",
                    s.name, s.volume
                )),
                SkipReason::SnapshotTree => {
                    match trees.iter_mut().find(|(v, _)| *v == s.volume.as_str()) {
                        Some((_, n)) => *n += 1,
                        None => trees.push((&s.volume, 1)),
                    }
                }
            }
        }
        for (volume, n) in trees {
            let what = if n == 1 { "subvolume" } else { "subvolumes" };
            r.push_str(&format!(
                "    {n} {what} inside snapshot trees skipped on {volume}\n"
            ));
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

    use crate::config::{Retention, Target};

    fn sv(name: &str) -> SubvolConfig {
        SubvolConfig {
            name: name.into(),
            ..Default::default()
        }
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
            retention: Retention {
                daily: 7,
                ..Default::default()
            },
            display_name: String::new(),
        }
    }

    /// Two volumes. `/ssd` has two sources (one to everything, one to the
    /// primary only); `/hdd` has one.
    fn config() -> Config {
        let mut c = Config {
            targets: vec![
                target("big", TargetRole::Primary),
                target("small", TargetRole::Mirror),
            ],
            sources: vec![
                source("ssd", "/ssd", &[], &["@srv", "@opt"]),
                source("ssd-vm", "/ssd", &["big"], &["@srv/VirtualMachines"]),
                source("media", "/hdd", &["big"], &["bosco-media"]),
            ],
            ..Config::default()
        };
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
            &[
                "@srv",
                "@opt",
                "@srv/VirtualMachines",
                "@srv/stremio-web",
                "@srv/VirtualMachines/win11",
            ],
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
        l[0] = listing(
            "/ssd",
            &["@srv", "@opt", "@srv/VirtualMachines", "@srv2", "@optional"],
        );
        let plan = plan_sync(&config(), &l);
        let names: Vec<_> = plan
            .adopt
            .iter()
            .map(|a| (a.name.as_str(), a.nested_under.clone()))
            .collect();
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
            &[
                "@srv",
                "@opt",
                "@srv/VirtualMachines",
                "@cache",
                "@cache/stremio",
                ".btrbk-snapshots/opt.20261001T0300",
                "@tmp",
            ],
        );
        let plan = plan_sync(&config(), &l);
        assert!(plan.adopt.is_empty(), "{:?}", plan.adopt);
        assert_eq!(
            plan.skipped,
            vec![
                Skip {
                    volume: "/ssd".into(),
                    name: ".btrbk-snapshots/opt.20261001T0300".into(),
                    reason: SkipReason::SnapshotTree
                },
                Skip {
                    volume: "/ssd".into(),
                    name: "@cache".into(),
                    reason: SkipReason::Excluded {
                        pattern: "@cache".into()
                    }
                },
                Skip {
                    volume: "/ssd".into(),
                    name: "@cache/stremio".into(),
                    reason: SkipReason::Excluded {
                        pattern: "@cache".into()
                    }
                },
                Skip {
                    volume: "/ssd".into(),
                    name: "@tmp".into(),
                    reason: SkipReason::Excluded {
                        pattern: "@tmp".into()
                    }
                },
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
        assert_eq!(
            plan.retire,
            vec![EntryRef {
                source_label: "ssd".into(),
                name: "@opt".into()
            }]
        );
        assert!(plan.revive.is_empty());

        let mut c = config();
        c.sources[0].subvolumes[1].retired = Some("2026-09-01".into()); // @opt
        // Still gone: already retired, nothing to do.
        assert_eq!(plan_sync(&c, &l), SyncPlan::default());
        // Back again: revived, and not adopted a second time.
        let plan = plan_sync(&c, &all_present());
        assert_eq!(
            plan.revive,
            vec![EntryRef {
                source_label: "ssd".into(),
                name: "@opt".into()
            }]
        );
        assert!(plan.adopt.is_empty());
    }

    #[test]
    fn rename_is_a_retire_plus_an_adopt() {
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@opt-new", "@srv/VirtualMachines"]);
        let plan = plan_sync(&config(), &l);
        assert_eq!(
            plan.retire,
            vec![EntryRef {
                source_label: "ssd".into(),
                name: "@opt".into()
            }]
        );
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].name, "@opt-new");
    }

    #[test]
    fn a_retired_ancestor_does_not_place_a_child() {
        let mut c = config();
        c.sources[0].subvolumes[0].retired = Some("2026-09-01".into()); // @srv
        let l = vec![
            listing("/ssd", &["@opt", "@srv/VirtualMachines", "@srv/x"]),
            listing("/hdd", &["bosco-media"]),
        ];
        let plan = plan_sync(&c, &l);
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].nested_under, None);
        assert_eq!(plan.adopt[0].source_label, "ssd-adopted");
    }

    #[test]
    fn a_failed_volume_changes_nothing_on_that_volume_only() {
        let l = vec![
            VolumeListing {
                volume: "/ssd".into(),
                subvolumes: Err("not mounted".into()),
            },
            listing("/hdd", &["bosco-media", "bosco-media/video"]),
        ];
        let plan = plan_sync(&config(), &l);
        assert_eq!(
            plan.failed_volumes,
            vec![("/ssd".into(), "not mounted".into())]
        );
        assert!(
            plan.retire.is_empty(),
            "nothing on /ssd may be retired: {:?}",
            plan.retire
        );
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].volume, "/hdd");
        assert!(plan.failed());
    }

    #[test]
    fn a_volume_with_no_listing_at_all_is_a_failed_volume() {
        let plan = plan_sync(
            &config(),
            &[listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines"])],
        );
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
        assert!(
            plan.unplaceable[0].why.contains("primary"),
            "{}",
            plan.unplaceable[0].why
        );
        // The nested one still has a home.
        assert_eq!(plan.adopt.len(), 1);
        assert!(plan.failed());
    }

    #[test]
    fn apply_adds_entries_with_explicit_unique_names_and_the_date() {
        let mut l = all_present();
        l[0] = listing(
            "/ssd",
            &[
                "@srv",
                "@opt",
                "@srv/VirtualMachines",
                "@srv/stremio-web",
                "opt",
                "@new",
            ],
        );
        let c = config();
        let plan = plan_sync(&c, &l);
        let out = apply_plan(&c, &plan, "2026-10-02").expect("consistent plan");

        let ssd = &out.sources[0];
        let web = ssd
            .subvolumes
            .iter()
            .find(|s| s.name == "@srv/stremio-web")
            .unwrap();
        assert_eq!(web.snapshot_name.as_deref(), Some("srv-stremio-web"));
        assert_eq!(web.adopted.as_deref(), Some("2026-10-02"));

        let adopted = out
            .sources
            .iter()
            .find(|s| s.label == "ssd-adopted")
            .unwrap();
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
        let once = apply_plan(&c, &plan_sync(&c, &l), "2026-10-02").expect("consistent plan");
        l[0] = listing(
            "/ssd",
            &["@srv", "@opt", "@srv/VirtualMachines", "@one", "@two"],
        );
        let twice =
            apply_plan(&once, &plan_sync(&once, &l), "2026-10-03").expect("consistent plan");
        assert_eq!(
            twice
                .sources
                .iter()
                .filter(|s| s.label == "ssd-adopted")
                .count(),
            1
        );
        let adopted = twice
            .sources
            .iter()
            .find(|s| s.label == "ssd-adopted")
            .unwrap();
        assert_eq!(adopted.subvolumes.len(), 2);
        // Running the planner on its own result finds nothing more to do.
        assert_eq!(plan_sync(&twice, &l), SyncPlan::default());
    }

    #[test]
    fn apply_stamps_retirement_and_clears_it_on_revival() {
        let c = config();
        let gone = vec![
            listing("/ssd", &["@srv", "@srv/VirtualMachines"]),
            listing("/hdd", &["bosco-media"]),
        ];
        let retired = apply_plan(&c, &plan_sync(&c, &gone), "2026-10-02").expect("consistent plan");
        let opt = retired.sources[0]
            .subvolumes
            .iter()
            .find(|s| s.name == "@opt")
            .unwrap();
        assert_eq!(opt.retired.as_deref(), Some("2026-10-02"));

        let back = apply_plan(&retired, &plan_sync(&retired, &all_present()), "2026-10-09")
            .expect("consistent plan");
        let opt = back.sources[0]
            .subvolumes
            .iter()
            .find(|s| s.name == "@opt")
            .unwrap();
        assert_eq!(opt.retired, None);
    }

    #[test]
    fn a_volume_whose_only_sources_are_adoption_sources_adopts_into_the_existing_one() {
        let mut c = config();
        // Every original entry on /hdd has been retired and removed; only the
        // adoption source is left.
        c.sources[2] = source("media-adopted", "/hdd", &["big"], &["old"]);
        let l = vec![
            listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines"]),
            listing("/hdd", &["old", "fresh"]),
        ];
        let plan = plan_sync(&c, &l);
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].source_label, "media-adopted");
        assert!(plan.unplaceable.is_empty());

        let out = apply_plan(&c, &plan, "2026-10-02").expect("consistent plan");
        assert_eq!(out.sources.len(), 3, "no second adoption source");
        let names: Vec<_> = out.sources[2]
            .subvolumes
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["old", "fresh"]);
    }

    fn entry(label: &str, name: &str) -> EntryRef {
        EntryRef {
            source_label: label.into(),
            name: name.into(),
        }
    }

    #[test]
    fn apply_refuses_a_retire_or_revive_that_names_no_entry() {
        let c = config();
        for (kind, plan) in [
            (
                "retire",
                SyncPlan {
                    retire: vec![entry("ssd", "@nope")],
                    ..Default::default()
                },
            ),
            (
                "revive",
                SyncPlan {
                    revive: vec![entry("ghost", "@opt")],
                    ..Default::default()
                },
            ),
        ] {
            let why = apply_plan(&c, &plan, "2026-10-02").unwrap_err();
            assert!(why.contains(kind), "{why}");
            let item = &plan.retire.iter().chain(&plan.revive).next().unwrap();
            assert!(
                why.contains(&item.source_label) && why.contains(&item.name),
                "{why}"
            );
        }
    }

    #[test]
    fn apply_refuses_an_adoption_on_a_volume_with_no_source() {
        let plan = SyncPlan {
            adopt: vec![Adoption {
                volume: "/nowhere".into(),
                name: "@lost".into(),
                source_label: "nowhere-adopted".into(),
                nested_under: None,
                manual_only: false,
            }],
            ..Default::default()
        };
        let why = apply_plan(&config(), &plan, "2026-10-02").unwrap_err();
        assert!(why.contains("@lost") && why.contains("/nowhere"), "{why}");
    }

    // --- Task 7: the sync shell ---

    use crate::fsutil::testing::Scripted;

    fn mounted(_: &Path) -> bool {
        true
    }
    fn unmounted(_: &Path) -> bool {
        false
    }

    fn healthy(volume: &str, uuid: &str, paths: &[&str]) -> Vec<(String, i32, String)> {
        let list: String = paths
            .iter()
            .map(|p| format!("ID 1 gen 1 top level 5 path {p}\n"))
            .collect();
        vec![
            (
                format!("findmnt -n -o UUID,FSROOT --target {volume}"),
                0,
                format!("{uuid} /\n"),
            ),
            (format!("btrfs subvolume list {volume}"), 0, list),
        ]
    }

    fn scripted(parts: Vec<Vec<(String, i32, String)>>) -> Scripted {
        Scripted::from_owned(parts.into_iter().flatten().collect())
    }

    #[test]
    fn parse_subvolume_paths_ignores_lines_that_are_not_listing_rows() {
        let out = "ERROR: cannot access path foo\n\
                   ID 256 gen 100 top level 5 path @\n\
                   WARNING: some path bar\n\
                   ID 257 gen 100 top level 5 path \n\
                   ID 258 gen 100 top level 5 path @home\n\
                   not ID 259 gen 1 top level 5 path sneaky\n";
        assert_eq!(parse_subvolume_paths(out), ["@", "@home"]);
    }

    #[test]
    fn list_volume_reads_a_mounted_top_level_volume_with_the_expected_uuid() {
        let r = scripted(vec![healthy("/ssd", "abc", &["@", "a b"])]);
        let l = list_volume(&r, &mounted, "/ssd", "UUID=abc");
        assert_eq!(l.subvolumes.unwrap(), ["@", "a b"]);
    }

    #[test]
    fn list_volume_refuses_an_unmounted_path_without_running_anything() {
        let r = Scripted::new(&[]);
        let l = list_volume(&r, &unmounted, "/ssd", "UUID=abc");
        assert!(l.subvolumes.unwrap_err().contains("not mounted"));
        assert!(r.calls().is_empty());
    }

    #[test]
    fn wrong_uuid_volume_is_not_listed() {
        let r = scripted(vec![healthy("/ssd", "OTHER", &["@"])]);
        let l = list_volume(&r, &mounted, "/ssd", "UUID=abc");
        let why = l.subvolumes.unwrap_err();
        assert!(why.contains("OTHER") && why.contains("abc"), "{why}");
        assert!(!r.calls().iter().any(|c| c.starts_with("btrfs")));
    }

    #[test]
    fn list_volume_refuses_a_volume_not_mounted_at_its_top_level() {
        let r = Scripted::new(&[("findmnt -n -o UUID,FSROOT --target /ssd", 0, "abc /@\n")]);
        let why = list_volume(&r, &mounted, "/ssd", "UUID=abc")
            .subvolumes
            .unwrap_err();
        assert!(why.contains("top level"), "{why}");
        assert!(!r.calls().iter().any(|c| c.starts_with("btrfs")));
    }

    #[test]
    fn list_volume_refuses_when_findmnt_names_no_filesystem_root() {
        // Output with a UUID but no FSROOT must not pass as a top-level mount.
        let r = Scripted::new(&[("findmnt -n -o UUID,FSROOT --target /ssd", 0, "abc\n")]);
        let why = list_volume(&r, &mounted, "/ssd", "UUID=abc")
            .subvolumes
            .unwrap_err();
        assert!(why.contains("top level"), "{why}");
    }

    #[test]
    fn list_volume_refuses_when_findmnt_fails() {
        let r = Scripted::new(&[("findmnt -n -o UUID,FSROOT --target /ssd", 1, "")]);
        let why = list_volume(&r, &mounted, "/ssd", "UUID=abc")
            .subvolumes
            .unwrap_err();
        assert!(why.contains("findmnt"), "{why}");
        assert!(!r.calls().iter().any(|c| c.starts_with("btrfs")));
    }

    #[test]
    fn list_volume_resolves_a_device_path_through_blkid() {
        let mut parts = healthy("/nvme", "n1", &["@"]);
        parts.push((
            "blkid -s UUID -o value /dev/nvme1n1p2".into(),
            0,
            "n1\n".into(),
        ));
        let l = list_volume(&scripted(vec![parts]), &mounted, "/nvme", "/dev/nvme1n1p2");
        assert_eq!(l.subvolumes.unwrap(), ["@"]);
        // blkid failing means the expected UUID is unknown: refuse.
        let l = list_volume(
            &scripted(vec![healthy("/nvme", "n1", &["@"])]),
            &mounted,
            "/nvme",
            "/dev/nvme1n1p2",
        );
        assert!(l.subvolumes.unwrap_err().contains("blkid"));
    }

    #[test]
    fn list_volume_refuses_when_blkid_prints_nothing() {
        // Exit 0 with empty output is not "no UUID is expected": refuse.
        let mut parts = healthy("/nvme", "n1", &["@"]);
        parts.push((
            "blkid -s UUID -o value /dev/nvme1n1p2".into(),
            0,
            "\n".into(),
        ));
        let r = scripted(vec![parts]);
        let why = list_volume(&r, &mounted, "/nvme", "/dev/nvme1n1p2")
            .subvolumes
            .unwrap_err();
        assert!(why.contains("no UUID"), "{why}");
        assert!(!r.calls().iter().any(|c| c.starts_with("btrfs")));
    }

    #[test]
    fn list_volume_reports_a_failed_or_empty_btrfs_listing() {
        let r = Scripted::new(&[
            ("findmnt -n -o UUID,FSROOT --target /ssd", 0, "abc /\n"),
            ("btrfs subvolume list /ssd", 1, ""),
        ]);
        assert!(
            list_volume(&r, &mounted, "/ssd", "UUID=abc")
                .subvolumes
                .unwrap_err()
                .contains("btrfs subvolume list")
        );
        let r = scripted(vec![healthy("/ssd", "abc", &[])]);
        assert!(
            list_volume(&r, &mounted, "/ssd", "UUID=abc")
                .subvolumes
                .unwrap_err()
                .contains("no subvolumes")
        );
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
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, &mounted).unwrap();
        assert!(out.written && !out.failed(), "{:?}", out.write_error);
        let saved = Config::load(&path).unwrap();
        assert!(
            saved.sources[0]
                .subvolumes
                .iter()
                .any(|s| s.name == "@srv/web")
        );
        let conf = std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap();
        assert!(
            conf.contains("subvolume             @srv/web\n    snapshot_name       srv-web\n"),
            "{conf}"
        );
    }

    #[test]
    fn sync_touches_nothing_when_nothing_changed_or_on_a_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();
        // "Nothing changed" includes btrbk.conf already saying what
        // config.toml renders to; an out-of-date one is regenerated (see
        // sync_regenerates_a_btrbk_conf_that_lacks_a_configured_entry).
        let rendered = crate::btrbk_conf::render_btrbk_conf(&Config::load(&path).unwrap());
        std::fs::write(dir.path().join("btrbk.conf"), &rendered).unwrap();

        let same = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &same, &mounted).unwrap();
        assert!(!out.written);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            rendered
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        std::fs::write(dir.path().join("btrbk.conf"), "OLD").unwrap();
        let more = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt", "@srv/web"])]);
        let out = sync_subvolumes(&path, true, "2026-10-02", &more, &mounted).unwrap();
        assert!(!out.written);
        assert_eq!(out.plan.adopt.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            "OLD"
        );
    }

    // --- btrbk.conf follows config.toml on every run, not only on changes ---

    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().ino()
    }

    #[test]
    fn sync_regenerates_a_btrbk_conf_that_lacks_a_configured_entry() {
        // An entry that reached config.toml some other way (the GUI helper, a
        // hand edit) and never reached btrbk.conf: nothing to adopt, but the
        // entry is not backed up until btrbk.conf names it.
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let conf = dir.path().join("btrbk.conf");
        let mut without = Config::load(&path).unwrap();
        without.sources[0].subvolumes.retain(|e| e.name != "@opt");
        std::fs::write(&conf, crate::btrbk_conf::render_btrbk_conf(&without)).unwrap();
        assert!(!std::fs::read_to_string(&conf).unwrap().contains("@opt"));
        let before = std::fs::read_to_string(&path).unwrap();

        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, &mounted).unwrap();
        assert!(!out.plan.changes_config(), "{:?}", out.plan);
        assert!(!out.failed(), "{:?}", out.write_error);
        assert_eq!(out.btrbk_conf, BtrbkConf::Regenerated);
        let written = std::fs::read_to_string(&conf).unwrap();
        assert_eq!(
            written,
            crate::btrbk_conf::render_btrbk_conf(&Config::load(&path).unwrap())
        );
        assert!(
            written.contains("subvolume             @opt\n"),
            "{written}"
        );
        // config.toml itself is not rewritten.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let text = format_sync_report(&out, false);
        assert_eq!(
            text,
            "SUBVOLUME SYNC\n  btrbk.conf was out of date with config.toml and has been regenerated.\n"
        );
    }

    #[test]
    fn sync_writes_a_btrbk_conf_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let conf = dir.path().join("btrbk.conf");
        std::fs::remove_file(&conf).unwrap();
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, &mounted).unwrap();
        assert_eq!(out.btrbk_conf, BtrbkConf::Regenerated);
        assert_eq!(
            std::fs::read_to_string(&conf).unwrap(),
            crate::btrbk_conf::render_btrbk_conf(&Config::load(&path).unwrap())
        );
    }

    #[test]
    fn sync_leaves_an_up_to_date_btrbk_conf_alone_and_says_nothing_about_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let conf = dir.path().join("btrbk.conf");
        let rendered = crate::btrbk_conf::render_btrbk_conf(&Config::load(&path).unwrap());
        std::fs::write(&conf, &rendered).unwrap();
        let ino = inode(&conf);

        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, &mounted).unwrap();
        assert_eq!(out.btrbk_conf, BtrbkConf::Current);
        // An atomic replace makes a new inode; the same one means no write.
        assert_eq!(inode(&conf), ino);
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), rendered);
        assert_eq!(
            format_sync_report(&out, false),
            "SUBVOLUME SYNC\n  No new, vanished or returning subvolumes.\n"
        );
    }

    #[test]
    fn a_dry_run_reports_an_out_of_date_btrbk_conf_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let conf = dir.path().join("btrbk.conf");
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt"])]);
        let out = sync_subvolumes(&path, true, "2026-10-02", &r, &mounted).unwrap();
        assert_eq!(out.btrbk_conf, BtrbkConf::WouldRegenerate);
        assert!(!out.failed());
        assert_eq!(std::fs::read_to_string(&conf).unwrap(), "OLD");
        let text = format_sync_report(&out, true);
        assert!(
            text.contains(
                "  btrbk.conf is out of date with config.toml and would be regenerated (dry run, nothing written).\n"
            ),
            "{text}"
        );
        assert!(!text.contains("No new, vanished"), "{text}");
    }

    #[test]
    fn a_btrbk_conf_that_cannot_be_regenerated_fails_the_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();
        // Occupy the temp-file name so the atomic write fails.
        std::fs::create_dir(dir.path().join(".btrbk.conf.tmp")).unwrap();
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, &mounted).unwrap();
        assert!(out.failed());
        assert_eq!(out.btrbk_conf, BtrbkConf::OutOfDate);
        assert!(
            out.write_error.as_deref().unwrap().contains("btrbk.conf"),
            "{:?}",
            out.write_error
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            "OLD"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let text = format_sync_report(&out, false);
        assert!(
            text.contains("  CONFIG NOT UPDATED: could not write"),
            "{text}"
        );
        assert!(
            text.contains(
                "  btrbk.conf is out of date with config.toml and was NOT regenerated.\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_failed_sync_reports_an_out_of_date_btrbk_conf_but_does_not_touch_it() {
        // While a volume cannot be read, sync changes neither file.
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let out =
            sync_subvolumes(&path, false, "2026-10-02", &Scripted::new(&[]), &unmounted).unwrap();
        assert!(out.failed());
        assert_eq!(out.btrbk_conf, BtrbkConf::OutOfDate);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            "OLD"
        );
        let text = format_sync_report(&out, false);
        assert!(
            text.contains(
                "  btrbk.conf is out of date with config.toml and was NOT regenerated.\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn an_invalid_config_toml_is_not_rendered_into_btrbk_conf() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        // Valid enough to load and list, not valid enough to act on.
        let mut c = Config::load(&path).unwrap();
        c.schedule.incremental = "not a time".into();
        assert!(!c.validate().is_empty(), "fixture must be invalid");
        c.save(&path).unwrap();
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, &mounted).unwrap();
        assert!(out.failed());
        assert!(
            out.write_error.as_deref().unwrap().contains("not valid"),
            "{:?}",
            out.write_error
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            "OLD"
        );
    }

    #[test]
    fn sync_keeps_both_files_when_the_new_config_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();
        // Occupy the temp-file name so the atomic write fails.
        std::fs::create_dir(dir.path().join(".btrbk.conf.tmp")).unwrap();
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt", "@srv/web"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, &mounted).unwrap();
        assert!(!out.written);
        assert!(out.failed());
        assert!(
            out.write_error.as_deref().unwrap().contains("btrbk.conf"),
            "{:?}",
            out.write_error
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            "OLD"
        );
    }

    #[test]
    fn sync_restores_btrbk_conf_when_config_toml_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();
        let old = Config::load(&path).unwrap();
        // btrbk.conf will write fine; the config.toml temp name is occupied.
        std::fs::create_dir(dir.path().join(".config.toml.tmp")).unwrap();
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt", "@srv/web"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, &mounted).unwrap();
        assert!(!out.written && out.failed());
        assert!(
            out.write_error.as_deref().unwrap().contains("config.toml"),
            "{:?}",
            out.write_error
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // Restored from the OLD config, so it still agrees with config.toml.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            crate::btrbk_conf::render_btrbk_conf(&old)
        );
    }

    #[test]
    fn a_plan_that_could_not_be_applied_writes_nothing_and_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();
        let old = Config::load(&path).unwrap();
        let why = write_updated(&old, Err("cannot retire '@x'".into()), &path);
        assert_eq!(why.as_deref(), Some("cannot retire '@x'"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            "OLD"
        );
    }

    #[test]
    fn an_invalid_updated_config_is_refused_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let before = std::fs::read_to_string(&path).unwrap();
        let old = Config::load(&path).unwrap();
        let mut bad = old.clone();
        bad.sources[0].device = String::new();
        assert!(!bad.validate().is_empty(), "fixture must be invalid");
        let why = write_updated(&old, Ok(bad), &path).unwrap();
        assert!(why.contains("not valid"), "{why}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            "OLD"
        );
    }

    #[test]
    fn a_valid_updated_config_is_written_by_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let old = Config::load(&path).unwrap();
        assert_eq!(write_updated(&old, Ok(old.clone()), &path), None);
        assert_ne!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            "OLD"
        );
    }

    #[test]
    fn sync_fails_when_a_volume_cannot_be_read_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let out =
            sync_subvolumes(&path, false, "2026-10-02", &Scripted::new(&[]), &unmounted).unwrap();
        assert!(out.failed() && !out.written);
        assert_eq!(out.plan.failed_volumes.len(), 1);
    }

    #[test]
    fn sync_errors_only_when_the_config_cannot_be_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let err = sync_subvolumes(
            &dir.path().join("absent.toml"),
            false,
            "2026-10-02",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap_err();
        assert!(err.contains("absent.toml"), "{err}");
    }

    #[test]
    fn sync_lists_each_shared_volume_once() {
        // Two sources on /ssd must cause one listing, not two.
        let dir = tempfile::tempdir().unwrap();
        let mut c = config();
        c.sources.truncate(2);
        for s in &mut c.sources {
            s.device = "UUID=abc".into();
        }
        c.general.btrbk_conf = dir.path().join("btrbk.conf").to_string_lossy().into_owned();
        let path = dir.path().join("config.toml");
        c.save(&path).unwrap();
        let r = scripted(vec![healthy(
            "/ssd",
            "abc",
            &["@srv", "@opt", "@srv/VirtualMachines"],
        )]);
        sync_subvolumes(&path, true, "2026-10-02", &r, &mounted).unwrap();
        let lists = r.calls().iter().filter(|c| c.starts_with("btrfs")).count();
        assert_eq!(lists, 1, "{:?}", r.calls());
    }

    #[test]
    fn report_lists_every_decision_and_says_when_nothing_changed() {
        let quiet = SyncOutcome {
            plan: SyncPlan::default(),
            written: false,
            write_error: None,
            btrbk_conf: BtrbkConf::Current,
            retire_refused: None,
        };
        assert_eq!(
            format_sync_report(&quiet, false),
            "SUBVOLUME SYNC\n  No new, vanished or returning subvolumes.\n"
        );

        let plan = SyncPlan {
            adopt: vec![
                Adoption {
                    volume: "/ssd".into(),
                    name: "@srv/web".into(),
                    source_label: "ssd".into(),
                    nested_under: Some("@srv".into()),
                    manual_only: false,
                },
                Adoption {
                    volume: "/ssd".into(),
                    name: "@new".into(),
                    source_label: "ssd-adopted".into(),
                    nested_under: None,
                    manual_only: false,
                },
            ],
            retire: vec![EntryRef {
                source_label: "ssd".into(),
                name: "@opt".into(),
            }],
            revive: vec![EntryRef {
                source_label: "ssd".into(),
                name: "@old".into(),
            }],
            skipped: vec![Skip {
                volume: "/ssd".into(),
                name: "@cache/x".into(),
                reason: SkipReason::Excluded {
                    pattern: "@cache".into(),
                },
            }],
            unplaceable: vec![Unplaceable {
                volume: "/hdd".into(),
                name: "top".into(),
                why: "no primary".into(),
            }],
            failed_volumes: vec![("/nvme".into(), "not mounted".into())],
        };
        let text = format_sync_report(
            &SyncOutcome {
                plan,
                written: true,
                write_error: None,
                btrbk_conf: BtrbkConf::Current,
                retire_refused: None,
            },
            false,
        );
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
            adopt: vec![Adoption {
                volume: "/v".into(),
                name: "a".into(),
                source_label: "s".into(),
                nested_under: None,
                manual_only: false,
            }],
            ..Default::default()
        };
        let dry = format_sync_report(
            &SyncOutcome {
                plan: plan.clone(),
                written: false,
                write_error: None,
                btrbk_conf: BtrbkConf::Current,
                retire_refused: None,
            },
            true,
        );
        assert!(
            dry.contains("  Would adopt (dry run, nothing written):\n"),
            "{dry}"
        );
        let bad = format_sync_report(
            &SyncOutcome {
                plan,
                written: false,
                write_error: Some("disk full".into()),
                btrbk_conf: BtrbkConf::Current,
                retire_refused: None,
            },
            false,
        );
        assert!(bad.contains("  CONFIG NOT UPDATED: disk full\n"), "{bad}");
        assert!(
            bad.contains("  NOT adopted (config could not be written):\n"),
            "{bad}"
        );
    }

    #[test]
    fn report_is_never_quiet_when_any_single_kind_of_finding_exists() {
        let quiet = "No new, vanished or returning subvolumes";
        let e = || EntryRef {
            source_label: "s".into(),
            name: "n".into(),
        };
        let each = [
            SyncPlan {
                retire: vec![e()],
                ..Default::default()
            },
            SyncPlan {
                revive: vec![e()],
                ..Default::default()
            },
            SyncPlan {
                skipped: vec![Skip {
                    volume: "/v".into(),
                    name: "n".into(),
                    reason: SkipReason::SnapshotTree,
                }],
                ..Default::default()
            },
            SyncPlan {
                unplaceable: vec![Unplaceable {
                    volume: "/v".into(),
                    name: "n".into(),
                    why: "w".into(),
                }],
                ..Default::default()
            },
            SyncPlan {
                failed_volumes: vec![("/v".into(), "w".into())],
                ..Default::default()
            },
        ];
        for plan in each {
            let text = format_sync_report(
                &SyncOutcome {
                    plan: plan.clone(),
                    written: true,
                    write_error: None,
                    btrbk_conf: BtrbkConf::Current,
                    retire_refused: None,
                },
                false,
            );
            assert!(!text.contains(quiet), "{plan:?}\n{text}");
        }
    }

    #[test]
    fn report_headings_follow_dry_run_then_written_then_not_written() {
        let plan = SyncPlan {
            retire: vec![EntryRef {
                source_label: "s".into(),
                name: "n".into(),
            }],
            revive: vec![EntryRef {
                source_label: "s".into(),
                name: "m".into(),
            }],
            skipped: vec![Skip {
                volume: "/v".into(),
                name: "k".into(),
                reason: SkipReason::SnapshotTree,
            }],
            ..Default::default()
        };
        let render = |written: bool, dry: bool| {
            format_sync_report(
                &SyncOutcome {
                    plan: plan.clone(),
                    written,
                    write_error: None,
                    btrbk_conf: BtrbkConf::Current,
                    retire_refused: None,
                },
                dry,
            )
        };
        // A dry run wins even if `written` were somehow set.
        let dry = render(true, true);
        assert!(
            dry.contains("  Would retire (dry run, nothing written):\n"),
            "{dry}"
        );
        assert!(
            dry.contains("  Would revive (dry run, nothing written):\n"),
            "{dry}"
        );
        let done = render(true, false);
        assert!(
            done.contains("  Retired (gone from disk; existing backups will expire):\n"),
            "{done}"
        );
        assert!(done.contains("  Revived (back on disk):\n"), "{done}");
        assert!(
            done.contains("    1 subvolume inside snapshot trees skipped on /v\n"),
            "{done}"
        );
        let not = render(false, false);
        assert!(
            not.contains("  NOT retired (config could not be written):\n"),
            "{not}"
        );
        assert!(
            not.contains("  NOT revived (config could not be written):\n"),
            "{not}"
        );
    }

    // --- fix round 1: every device a volume's sources name must verify ---

    /// `config()` with each source's device set, in source order
    /// (ssd, ssd-vm, media).
    fn config_with_devices(devices: &[&str]) -> Config {
        let mut c = config();
        for (source, device) in c.sources.iter_mut().zip(devices) {
            source.device = (*device).into();
        }
        c
    }

    #[test]
    fn list_volumes_lists_a_volume_once_when_its_sources_share_a_device() {
        let c = config_with_devices(&["UUID=abc", "UUID=abc", "UUID=zzz"]);
        let r = scripted(vec![
            healthy("/ssd", "abc", &["@srv", "@opt"]),
            healthy("/hdd", "zzz", &["bosco-media"]),
        ]);
        let l = list_volumes(&c, &r, &mounted);
        assert_eq!(l.len(), 2);
        assert_eq!(
            (l[0].volume.as_str(), l[1].volume.as_str()),
            ("/ssd", "/hdd")
        );
        assert_eq!(l[0].subvolumes.as_ref().unwrap(), &["@srv", "@opt"]);
        let lists = r
            .calls()
            .iter()
            .filter(|c| *c == "btrfs subvolume list /ssd")
            .count();
        assert_eq!(lists, 1, "{:?}", r.calls());
    }

    #[test]
    fn list_volumes_fails_a_volume_when_a_later_source_names_another_filesystem() {
        let c = config_with_devices(&["UUID=abc", "UUID=other", "UUID=zzz"]);
        let r = scripted(vec![
            healthy("/ssd", "abc", &["@srv", "@opt"]),
            healthy("/hdd", "zzz", &["bosco-media"]),
        ]);
        let l = list_volumes(&c, &r, &mounted);
        let why = l[0].subvolumes.as_ref().unwrap_err();
        assert!(
            why.contains("ssd-vm")
                && why.contains("UUID=other")
                && why.contains("expected 'other'"),
            "{why}"
        );
        // The failure is confined to the volume it belongs to.
        assert!(l[1].subvolumes.is_ok());
    }

    #[test]
    fn a_mismatched_second_device_changes_nothing_through_sync() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = config_with_devices(&["UUID=abc", "UUID=other"]);
        c.sources.truncate(2);
        c.general.btrbk_conf = dir.path().join("btrbk.conf").to_string_lossy().into_owned();
        let path = dir.path().join("config.toml");
        c.save(&path).unwrap();
        std::fs::write(dir.path().join("btrbk.conf"), "OLD").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        // Were the first device trusted, @new would be adopted and
        // @srv/VirtualMachines (ssd-vm's) retired.
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@opt", "@new"])]);
        let out = sync_subvolumes(&path, false, "2026-10-02", &r, &mounted).unwrap();
        assert!(out.failed() && !out.written);
        assert!(
            out.plan.adopt.is_empty() && out.plan.retire.is_empty(),
            "{:?}",
            out.plan
        );
        assert_eq!(out.plan.failed_volumes.len(), 1);
        assert!(out.plan.failed_volumes[0].1.contains("ssd-vm"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
            "OLD"
        );
    }

    #[test]
    fn list_volumes_accepts_two_spellings_of_the_same_filesystem() {
        let c = config_with_devices(&["UUID=abc", "/dev/sdx", "UUID=zzz"]);
        let mut parts = healthy("/ssd", "abc", &["@srv", "@opt"]);
        parts.push(("blkid -s UUID -o value /dev/sdx".into(), 0, "abc\n".into()));
        parts.extend(healthy("/hdd", "zzz", &["bosco-media"]));
        let l = list_volumes(&c, &scripted(vec![parts]), &mounted);
        assert_eq!(l[0].subvolumes.as_ref().unwrap(), &["@srv", "@opt"]);
    }

    #[test]
    fn list_volumes_keeps_order_and_lists_the_healthy_volume_beside_a_failing_one() {
        let c = config_with_devices(&["UUID=abc", "UUID=abc", "UUID=zzz"]);
        // Nothing scripted for /ssd: it fails; /hdd is still read.
        let r = scripted(vec![healthy("/hdd", "zzz", &["bosco-media"])]);
        let l = list_volumes(&c, &r, &mounted);
        assert_eq!(
            (l[0].volume.as_str(), l[1].volume.as_str()),
            ("/ssd", "/hdd")
        );
        assert!(l[0].subvolumes.is_err());
        assert_eq!(l[1].subvolumes.as_ref().unwrap(), &["bosco-media"]);
    }

    #[test]
    fn list_volumes_reports_the_first_failing_device_and_a_later_success_never_clears_it() {
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv"])]);
        // First source wrong, second right: still an error, naming the first.
        let c = config_with_devices(&["UUID=bad", "UUID=abc", "UUID=zzz"]);
        let why = list_volumes(&c, &r, &mounted)[0]
            .subvolumes
            .clone()
            .unwrap_err();
        assert!(
            why.contains("source 'ssd'") && why.contains("UUID=bad"),
            "{why}"
        );
        // Both wrong: the first failure is the one reported.
        let c = config_with_devices(&["UUID=bad", "UUID=worse", "UUID=zzz"]);
        let why = list_volumes(&c, &r, &mounted)[0]
            .subvolumes
            .clone()
            .unwrap_err();
        assert!(
            why.contains("UUID=bad") && !why.contains("UUID=worse"),
            "{why}"
        );
    }

    // --- a retirement is never stamped from a clock that cannot be trusted ---

    #[test]
    fn a_clock_before_2026_refuses_to_retire_but_still_adopts() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        // @opt vanished, @srv/web appeared.
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv", "@srv/web"])]);
        let out = sync_subvolumes(&path, false, "1970-01-01", &r, &mounted).unwrap();
        assert!(out.failed());
        assert!(out.written, "{:?}", out.write_error);
        let saved = Config::load(&path).unwrap();
        let entry = |name: &str| {
            saved.sources[0]
                .subvolumes
                .iter()
                .find(|e| e.name == name)
                .cloned()
        };
        assert_eq!(
            entry("@opt").unwrap().retired,
            None,
            "no retirement stamped"
        );
        assert!(entry("@srv/web").is_some(), "adoption still happens");
        let text = format_sync_report(&out, false);
        assert!(
            text.contains("  NOT retired (the system clock reads 1970-01-01"),
            "{text}"
        );
        assert!(text.contains("    @opt  [source ssd]\n"), "{text}");
        assert!(text.contains("  Adopted (now backed up):\n"), "{text}");
    }

    #[test]
    fn a_trusted_clock_retires_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let path = on_disk_config(dir.path());
        let r = scripted(vec![healthy("/ssd", "abc", &["@srv"])]);
        let out = sync_subvolumes(&path, false, "2026-01-01", &r, &mounted).unwrap();
        assert!(!out.failed() && out.written, "{:?}", out.write_error);
        let saved = Config::load(&path).unwrap();
        assert_eq!(
            saved.sources[0].subvolumes[1].retired.as_deref(),
            Some("2026-01-01")
        );
    }

    // --- snapshot-tree skips are counted, exclusions itemised (Ruling 28) ---

    #[test]
    fn snapshot_tree_skips_are_one_count_per_volume_and_exclusions_stay_itemised() {
        let skip = |volume: &str, name: &str, reason: SkipReason| Skip {
            volume: volume.into(),
            name: name.into(),
            reason,
        };
        let tree = || SkipReason::SnapshotTree;
        let plan = SyncPlan {
            skipped: vec![
                skip(
                    "/ssd",
                    "@cache/x",
                    SkipReason::Excluded {
                        pattern: "@cache".into(),
                    },
                ),
                skip("/ssd", ".btrbk-snapshots/srv.20261001", tree()),
                skip("/ssd", ".btrbk-snapshots/srv.20261002", tree()),
                skip("/hdd", ".snapshots/1/snapshot", tree()),
                skip("/ssd", "@srv/.snapshots/1/snapshot", tree()),
                skip(
                    "/hdd",
                    "@tmp",
                    SkipReason::Excluded {
                        pattern: "@tmp".into(),
                    },
                ),
            ],
            ..Default::default()
        };
        let text = format_sync_report(
            &SyncOutcome {
                plan,
                ..Default::default()
            },
            false,
        );
        assert_eq!(
            text,
            "SUBVOLUME SYNC\n\
             \x20 Skipped:\n\
             \x20   @cache/x  [/ssd, excluded by '@cache']\n\
             \x20   @tmp  [/hdd, excluded by '@tmp']\n\
             \x20   3 subvolumes inside snapshot trees skipped on /ssd\n\
             \x20   1 subvolume inside snapshot trees skipped on /hdd\n"
        );
        // No snapshot name is listed any more.
        assert!(!text.contains("srv.2026"), "{text}");
    }

    #[test]
    fn an_untrusted_clock_refuses_retirement_on_every_return_path() {
        // Retirement alone (nothing else changes the config), and a dry run
        // that would also adopt: both must carry the refusal and fail.
        for (dry_run, listed) in [(false, &["@srv"][..]), (true, &["@srv", "@srv/web"][..])] {
            let dir = tempfile::tempdir().unwrap();
            let path = on_disk_config(dir.path());
            let before = std::fs::read_to_string(&path).unwrap();
            let r = scripted(vec![healthy("/ssd", "abc", listed)]);
            let out = sync_subvolumes(&path, dry_run, "1970-01-01", &r, &mounted).unwrap();
            assert!(
                out.retire_refused
                    .as_deref()
                    .is_some_and(|w| w.contains("1970-01-01")),
                "dry_run={dry_run}: {out:?}"
            );
            assert!(out.failed(), "dry_run={dry_run}");
            assert!(!out.written, "dry_run={dry_run}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
    }
}
