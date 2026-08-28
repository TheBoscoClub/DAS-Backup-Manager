//! Deleting snapshots from backup targets.
//!
//! Backs two commands that differ only in how they SELECT snapshots:
//!
//! * `btrdasd forget <glob>` (`bd DAS-Backup-Manager-u8n`) — selects by series
//!   name. For snapshots stranded when a subvolume is renamed: the old
//!   `snapshot_name` matches no retention rule, so btrbk never prunes them
//!   (`btrbk clean` handles incomplete/garbled only, `btrbk prune` matches
//!   configured names) and they accumulate forever.
//! * `btrdasd purge <glob>` (`bd DAS-Backup-Manager-rt6`) — selects by file
//!   path, via the index, then deletes every snapshot containing it.
//!
//! # Why purge is snapshot-granular, and always will be
//!
//! Removing a file from inside a backup is not a missing feature, it is
//! impossible. Target snapshots are received read-only subvolumes; deleting a
//! file requires clearing `ro`, which permanently destroys the Received UUID
//! that only `btrfs receive` can set. Per `.claude/rules/backup.md` that leaves
//! "a subvolume that looks like a valid replica while its content may drift",
//! and silently disqualifies it as an incremental parent. Mutating an
//! already-sent LOCAL parent is equally unsafe: `btrfs send -p` diffs against it
//! and never re-emits what the target is missing.
//!
//! So the only sound removal is the whole snapshot, and the cost — losing those
//! restore points, and a full re-send afterwards — is stated up front rather
//! than discovered.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::db::Snapshot;
use crate::doctor::glob_match;

/// Why a plan refused to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetRefusal {
    /// The pattern matches a series btrbk is still backing up.
    LiveSeries(String),
    /// The pattern matched nothing.
    NoMatch,
}

/// One snapshot selected for deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetCandidate {
    pub id: i64,
    pub series: String,
    pub path: PathBuf,
}

/// What a forget/purge would delete, before anything is touched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ForgetPlan {
    pub candidates: Vec<ForgetCandidate>,
    /// Distinct series the candidates belong to, for the summary.
    pub series: BTreeSet<String>,
}

impl ForgetPlan {
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }
}

/// Snapshot names btrbk is currently configured to produce.
///
/// Read from the generated `btrbk.conf` rather than re-derived from
/// `config.toml`: that file is what btrbk actually reads, so it cannot disagree
/// with reality the way a second implementation of the naming rules could.
pub fn live_snapshot_names(btrbk_conf: &Path) -> std::io::Result<Vec<String>> {
    let text = std::fs::read_to_string(btrbk_conf)?;
    Ok(parse_live_snapshot_names(&text))
}

/// Pure half of [`live_snapshot_names`].
pub fn parse_live_snapshot_names(conf: &str) -> Vec<String> {
    conf.lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("snapshot_name"))
        .map(|rest| rest.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect()
}

/// Map each `subvolume` declared in `btrbk.conf` to the `snapshot_name` btrbk
/// will actually write for it.
///
/// Same rationale as [`live_snapshot_names`], and the same source of truth: the
/// generated file btrbk itself consumes. Re-deriving the mapping from
/// `config.toml` would reintroduce the second-implementation drift that
/// `DAS-Backup-Manager-5ig` was — `resolve_snapshot_names` disambiguates a bare
/// `@` to `root-` when another subvolume also resolves to `root`, and nothing
/// outside that function can predict when that happens.
pub fn live_subvol_snapshot_names(
    btrbk_conf: &Path,
) -> std::io::Result<std::collections::HashMap<String, String>> {
    let text = std::fs::read_to_string(btrbk_conf)?;
    Ok(parse_subvol_snapshot_names(&text))
}

/// Pure half of [`live_subvol_snapshot_names`].
///
/// btrbk.conf nests `snapshot_name` under the `subvolume` it belongs to:
///
/// ```text
/// subvolume             @
///   snapshot_name       root-
/// ```
///
/// A `subvolume` with no `snapshot_name` of its own is absent from the map
/// rather than guessed at — callers must treat "not present" as "do not act",
/// never as "use a default".
pub fn parse_subvol_snapshot_names(conf: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut current: Option<String> = None;
    for line in conf.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("subvolume") {
            let name = rest.trim();
            current = (!name.is_empty()).then(|| name.to_string());
        } else if let Some(rest) = line.strip_prefix("snapshot_name") {
            let value = rest.trim();
            if let Some(subvol) = current.as_ref()
                && !value.is_empty()
            {
                map.insert(subvol.clone(), value.to_string());
            }
        } else if line.starts_with("volume ") {
            // A new volume block ends the previous subvolume's scope.
            current = None;
        }
    }
    map
}

/// Select snapshots whose SERIES NAME matches `pattern`.
///
/// Refuses outright if the pattern also matches a series btrbk still writes —
/// the guard that stops `ClaudeCodeProjects-claude-cowork*` from taking the live
/// `...-desktop-maintenance` chain along with the stranded `...-linux` one.
pub fn plan_forget(
    snapshots: &[Snapshot],
    pattern: &str,
    live_names: &[String],
) -> Result<ForgetPlan, ForgetRefusal> {
    if let Some(hit) = live_names.iter().find(|n| glob_match(pattern, n)) {
        return Err(ForgetRefusal::LiveSeries(hit.clone()));
    }
    let plan = collect(snapshots.iter().filter(|s| glob_match(pattern, &s.name)));
    if plan.is_empty() {
        return Err(ForgetRefusal::NoMatch);
    }
    Ok(plan)
}

/// Select snapshots by id, for purge — which resolves ids through the index
/// rather than by name.
///
/// The live-series guard does NOT apply here and must not: purging a leaked
/// credential means removing it from the series that is still being backed up.
/// That is the whole point, and it is why purge states the re-send cost up front.
pub fn plan_purge(snapshots: &[Snapshot], ids: &[i64]) -> Result<ForgetPlan, ForgetRefusal> {
    let wanted: BTreeSet<i64> = ids.iter().copied().collect();
    let plan = collect(snapshots.iter().filter(|s| wanted.contains(&s.id)));
    if plan.is_empty() {
        return Err(ForgetRefusal::NoMatch);
    }
    Ok(plan)
}

fn collect<'a>(it: impl Iterator<Item = &'a Snapshot>) -> ForgetPlan {
    let mut plan = ForgetPlan::default();
    for snap in it {
        plan.series.insert(snap.name.clone());
        plan.candidates.push(ForgetCandidate {
            id: snap.id,
            series: snap.name.clone(),
            path: PathBuf::from(&snap.path),
        });
    }
    plan
}

/// Delete one read-only snapshot subvolume.
///
/// `btrfs subvolume delete` only — never `rm -rf`, and never a `ro` flip.
pub fn delete_subvolume(path: &Path) -> Result<(), String> {
    let out = std::process::Command::new("btrfs")
        .args(["subvolume", "delete"])
        .arg(path)
        .output()
        .map_err(|e| format!("could not run btrfs: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: i64, name: &str, path: &str) -> Snapshot {
        Snapshot {
            id,
            name: name.to_string(),
            ts: "20260824T0300".to_string(),
            source: "projects".to_string(),
            path: path.to_string(),
            indexed_at: 0,
        }
    }

    fn fixture() -> Vec<Snapshot> {
        vec![
            snap(
                1,
                "Proj-cowork-linux",
                "/mnt/t/projects/Proj-cowork-linux.20260822",
            ),
            snap(
                2,
                "Proj-cowork-linux",
                "/mnt/t/projects/Proj-cowork-linux.20260823",
            ),
            snap(
                3,
                "Proj-cowork-desktop",
                "/mnt/t/projects/Proj-cowork-desktop.20260824",
            ),
            snap(4, "audiobooks", "/mnt/t/audiobooks/audiobooks.20260824"),
        ]
    }

    #[test]
    fn forget_refuses_a_pattern_that_reaches_a_live_series() {
        // THE guard. `*cowork*` also matches the series btrbk is still writing,
        // so it must refuse rather than delete the live chain alongside the
        // stranded one.
        let live = vec!["Proj-cowork-desktop".to_string()];
        let err = plan_forget(&fixture(), "Proj-cowork*", &live).unwrap_err();
        assert_eq!(err, ForgetRefusal::LiveSeries("Proj-cowork-desktop".into()));
    }

    #[test]
    fn forget_selects_only_the_stranded_series() {
        let live = vec!["Proj-cowork-desktop".to_string()];
        let plan = plan_forget(&fixture(), "Proj-cowork-linux", &live).unwrap();
        assert_eq!(plan.candidates.len(), 2);
        assert!(
            plan.candidates
                .iter()
                .all(|c| c.series == "Proj-cowork-linux")
        );
        assert_eq!(plan.series.len(), 1);
    }

    #[test]
    fn forget_reports_no_match_rather_than_deleting_nothing_quietly() {
        let live = vec![];
        assert_eq!(
            plan_forget(&fixture(), "nothing-like-this", &live).unwrap_err(),
            ForgetRefusal::NoMatch
        );
    }

    #[test]
    fn forget_guard_is_not_defeated_by_an_exact_live_name() {
        let live = vec!["audiobooks".to_string()];
        assert_eq!(
            plan_forget(&fixture(), "audiobooks", &live).unwrap_err(),
            ForgetRefusal::LiveSeries("audiobooks".into())
        );
    }

    #[test]
    fn purge_selects_by_id_and_ignores_the_live_guard() {
        // Purging a leaked secret must be able to touch the live series — that
        // is the case it exists for.
        let plan = plan_purge(&fixture(), &[3, 4]).unwrap();
        assert_eq!(plan.candidates.len(), 2);
        assert!(plan.series.contains("Proj-cowork-desktop"));
        assert!(plan.series.contains("audiobooks"));
    }

    #[test]
    fn purge_with_no_matching_ids_refuses() {
        assert_eq!(
            plan_purge(&fixture(), &[999]).unwrap_err(),
            ForgetRefusal::NoMatch
        );
    }

    #[test]
    fn parses_snapshot_names_from_a_btrbk_config() {
        let conf = "\
volume /.btrfs-hdd
  snapshot_dir .btrbk-snapshots
  subvolume             ClaudeCodeProjects/adaptive-tuning-agent
    snapshot_name       ClaudeCodeProjects-adaptive-tuning-agent

  subvolume             ClaudeCodeProjects/the-last-shave
    snapshot_name       ClaudeCodeProjects-the-last-shave
";
        assert_eq!(
            parse_live_snapshot_names(conf),
            vec![
                "ClaudeCodeProjects-adaptive-tuning-agent".to_string(),
                "ClaudeCodeProjects-the-last-shave".to_string(),
            ]
        );
    }

    #[test]
    fn ignores_lines_that_merely_start_similarly() {
        // `snapshot_dir` must not be read as a `snapshot_name`.
        let conf = "  snapshot_dir .btrbk-snapshots\n  snapshot_preserve_min 2d\n";
        assert!(parse_live_snapshot_names(conf).is_empty());
    }
}
