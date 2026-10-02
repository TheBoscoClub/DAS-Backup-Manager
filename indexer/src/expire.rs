//! Expiry of the backups a retired subvolume left behind.
//!
//! Rule (spec §9): every snapshot of a retired series stays on a target until
//! the retirement date plus that target's longest retention window, then all
//! of them are deleted from that target together. btrbk cannot do this:
//! `btrbk prune` skips deletion when the source is not accessible.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::btrbk_conf::resolve_snapshot_names;
use crate::caldate::{date_of, day_number, untrusted_clock};
use crate::config::{Config, Retention, Source, Target, TargetRole};
use crate::fsutil::CommandRunner;

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
            date.len() == 8
                && all_digits(date)
                && (time.len() == 4 || time.len() == 6)
                && all_digits(time)
        }
        None => stamp.len() == 8 && all_digits(stamp),
    }
}

/// The day number of the date a series snapshot's name carries
/// (`<snapshot_name>.YYYYMMDD…`), or `None` if it carries none.
fn snapshot_day(entry: &str, snapshot_name: &str) -> Option<i64> {
    let stamp = entry.strip_prefix(snapshot_name)?.strip_prefix('.')?;
    let (y, m, d) = (stamp.get(0..4)?, stamp.get(4..6)?, stamp.get(6..8)?);
    day_number(&format!("{y}-{m}-{d}"))
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

/// Entries that start like a snapshot of this series — `<snapshot_name>.`
/// followed by at least 8 digits — but that `series_snapshots` does not
/// recognise (a timezone suffix, a `.partial` leftover, a naming this code
/// does not know). Sorted. A directory holding any of these is not understood,
/// so nothing there may be deleted and the series is not "gone".
pub fn unrecognised_series_entries(entries: &[String], snapshot_name: &str) -> Vec<String> {
    if snapshot_name.is_empty() {
        return Vec::new();
    }
    let recognised = series_snapshots(entries, snapshot_name);
    let mut found: Vec<String> = entries
        .iter()
        .filter(|entry| {
            entry
                .strip_prefix(snapshot_name)
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(|rest| rest.bytes().take_while(u8::is_ascii_digit).count() >= 8)
        })
        .filter(|entry| !recognised.contains(entry))
        .cloned()
        .collect();
    found.sort();
    found
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationState {
    /// The directory could not be examined (target not attached, not a mountpoint, unreadable).
    Unreachable(String),
    /// Snapshots remain; `expires` is None when the target has no retention window.
    Kept {
        count: usize,
        expires: Option<String>,
    },
    Deleted {
        count: usize,
    },
    DeleteFailed {
        deleted: usize,
        errors: Vec<String>,
    },
    /// Entries named like this series that are not recognised as btrbk
    /// snapshots. Nothing is deleted here, and the location is not "gone".
    Unrecognised {
        count: usize,
        example: String,
    },
    /// A live entry uses the same snapshot name in this same directory, so
    /// its snapshots cannot be told apart from this retired series'. Nothing
    /// is deleted here.
    Shared {
        source_label: String,
        name: String,
    },
    Empty,
    /// The newest snapshot of the series is dated more than a day after the
    /// retirement date, so that date is wrong — the clock was wrong when it
    /// was stamped, or it was edited. Nothing is deleted here.
    RetiredBeforeNewest {
        count: usize,
        newest: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationReport {
    pub place: String,
    pub state: LocationState,
    pub deleted_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredReport {
    pub source_label: String,
    pub name: String,
    pub snapshot_name: String,
    pub retired: String,
    pub locations: Vec<LocationReport>,
    pub removed_from_config: bool,
    /// Why an otherwise removable entry was kept, worded for the report.
    pub kept_reason: Option<String>,
}

pub struct ExpireOutcome {
    pub entries: Vec<RetiredReport>,
    /// Why a fully expired entry could not be removed from the config.
    pub config_error: Option<String>,
    /// Why nothing was judged expired: the system clock cannot be trusted
    /// (`caldate::untrusted_clock`). Set only when a retired entry exists.
    pub clock_error: Option<String>,
}

impl ExpireOutcome {
    /// A delete that failed, or a config that could not be saved. An
    /// unreachable target is not a failure: targets are allowed to be absent.
    pub fn failed(&self) -> bool {
        self.config_error.is_some()
            || self.clock_error.is_some()
            || self
                .entries
                .iter()
                .flat_map(|e| &e.locations)
                .any(|l| matches!(l.state, LocationState::DeleteFailed { .. }))
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

/// The first LIVE entry, in any source accepted by `same_place`, that
/// resolves to `snapshot_name`. Names are resolved within the entry's own
/// source, as btrbk will see them.
fn live_user_of(
    config: &Config,
    snapshot_name: &str,
    same_place: impl Fn(&Source) -> bool,
) -> Option<(String, String)> {
    config
        .sources
        .iter()
        .filter(|other| same_place(other))
        .find_map(|other| {
            let names = resolve_snapshot_names(&other.subvolumes);
            other
                .subvolumes
                .iter()
                .zip(names)
                .find(|(e, n)| e.retired.is_none() && n == snapshot_name)
                .map(|(e, _)| (other.label.clone(), e.name.clone()))
        })
}

fn target_subdir(source: &Source) -> &String {
    source.target_subdirs.first().unwrap_or(&source.label)
}

/// Every entry name of a directory, or why the listing cannot be trusted. A
/// read error part-way, or a name that is not valid UTF-8, would otherwise
/// make a directory look emptier than it is, and an "empty" location lets the
/// entry leave the config.
fn collect_names(
    entries: impl Iterator<Item = std::io::Result<std::ffi::OsString>>,
    dir: &Path,
) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for entry in entries {
        let name =
            entry.map_err(|e| format!("{} could not be read completely: {e}", dir.display()))?;
        match name.into_string() {
            Ok(name) => names.push(name),
            Err(raw) => {
                return Err(format!(
                    "{} holds an entry whose name is not valid UTF-8 ({})",
                    dir.display(),
                    raw.to_string_lossy()
                ));
            }
        }
    }
    Ok(names)
}

#[allow(clippy::too_many_arguments)]
fn examine(
    place: String,
    mount_root: &Path,
    dir: &Path,
    snapshot_name: &str,
    retired: &str,
    window: Option<u32>,
    today: Option<&str>,
    dry_run: bool,
    runner: &dyn CommandRunner,
    is_mountpoint: &dyn Fn(&Path) -> bool,
    shared_with: Option<(String, String)>,
) -> LocationReport {
    let report = |state| LocationReport {
        place: place.clone(),
        state,
        deleted_paths: Vec::new(),
    };
    if !is_mountpoint(mount_root) {
        return report(LocationState::Unreachable(format!(
            "{} is not mounted",
            mount_root.display()
        )));
    }
    // A live series in this very directory: its snapshots carry the same
    // names, so nothing here can be attributed to the retired one alone.
    if let Some((source_label, name)) = shared_with {
        return report(LocationState::Shared { source_label, name });
    }
    let entries: Vec<String> = match std::fs::read_dir(dir) {
        Ok(read) => match collect_names(read.map(|e| e.map(|e| e.file_name())), dir) {
            Ok(names) => names,
            Err(why) => return report(LocationState::Unreachable(why)),
        },
        // A target that never received this source has no such directory.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return report(LocationState::Unreachable(format!(
                "{} could not be read: {e}",
                dir.display()
            )));
        }
    };
    // Checked before anything else, whatever the window says: if the naming
    // here is not understood, nothing here is deleted.
    let unrecognised = unrecognised_series_entries(&entries, snapshot_name);
    if let Some(example) = unrecognised.first() {
        return report(LocationState::Unrecognised {
            count: unrecognised.len(),
            example: example.clone(),
        });
    }
    let snapshots = series_snapshots(&entries, snapshot_name);
    if snapshots.is_empty() {
        return report(LocationState::Empty);
    }
    // Snapshot names carry local time and retirement dates UTC, so one day
    // apart is ordinary. More than that means the retirement date is wrong,
    // and a wrong date must never be the reason a backup is deleted.
    let newest = snapshots
        .iter()
        .filter_map(|name| snapshot_day(name, snapshot_name))
        .max();
    if let (Some(newest), Some(retired_day)) = (newest, day_number(retired))
        && newest > retired_day + 1
    {
        return report(LocationState::RetiredBeforeNewest {
            count: snapshots.len(),
            newest: date_of(newest),
        });
    }
    // An untrusted clock decides nothing: the series is kept, with its date.
    let due = match today {
        Some(today) => window.and_then(|w| is_expired(retired, w, today)),
        None => window.map(|_| false),
    };
    if due != Some(true) {
        return report(LocationState::Kept {
            count: snapshots.len(),
            // No date when the window or the retirement date is unknown:
            // such a series is kept until someone decides.
            expires: window
                .filter(|_| due.is_some())
                .and_then(|w| expiry_date(retired, w)),
        });
    }
    if dry_run {
        return report(LocationState::Deleted {
            count: snapshots.len(),
        });
    }
    let mut deleted_paths = Vec::new();
    let mut errors = Vec::new();
    for name in &snapshots {
        let path = dir.join(name);
        match runner.output(
            Command::new("btrfs")
                .args(["subvolume", "delete"])
                .arg(&path),
        ) {
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
        LocationState::Deleted {
            count: deleted_paths.len(),
        }
    } else {
        LocationState::DeleteFailed {
            deleted: deleted_paths.len(),
            errors,
        }
    };
    LocationReport {
        place,
        state,
        deleted_paths,
    }
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
    let clock_error = untrusted_clock(today);
    // `None`: nothing may be judged expired against this clock.
    let trusted_today = clock_error.is_none().then_some(today);
    let mut entries = Vec::new();

    for source in &config.sources {
        let names = resolve_snapshot_names(&source.subvolumes);
        for (entry, snapshot_name) in source.subvolumes.iter().zip(names) {
            let Some(retired) = entry.retired.clone() else {
                continue;
            };
            // Back on its (mounted) source volume: sync will revive it, and
            // until it has, none of its backups may go. Sync may have failed
            // to read the volume this run, or not run at all.
            let volume = Path::new(&source.volume);
            if is_mountpoint(volume) && volume.join(&entry.name).exists() {
                entries.push(RetiredReport {
                    source_label: source.label.clone(),
                    name: entry.name.clone(),
                    snapshot_name,
                    retired,
                    locations: Vec::new(),
                    removed_from_config: false,
                    kept_reason: Some(
                        "subvolume exists again; waiting for sync to revive it — nothing deleted"
                            .into(),
                    ),
                });
                continue;
            }
            let targets = targets_of(&config, source);
            let subdir = target_subdir(source);
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
                    trusted_today,
                    dry_run,
                    runner,
                    is_mountpoint,
                    live_user_of(&config, &snapshot_name, |other| {
                        target_subdir(other) == subdir
                            && targets_of(&config, other)
                                .iter()
                                .any(|t| t.label == target.label)
                    }),
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
                trusted_today,
                dry_run,
                runner,
                is_mountpoint,
                live_user_of(&config, &snapshot_name, |other| {
                    other.volume == source.volume && other.snapshot_dir == source.snapshot_dir
                }),
            ));
            entries.push(RetiredReport {
                source_label: source.label.clone(),
                name: entry.name.clone(),
                snapshot_name,
                retired,
                locations,
                removed_from_config: false,
                // Nothing was examined on any target, so "empty everywhere"
                // would only mean the source side is empty.
                kept_reason: targets.is_empty().then(|| {
                    format!(
                        "no configured target receives source {} — entry kept",
                        source.label
                    )
                }),
            });
        }
    }

    // An entry is removable when nothing of its own remains in any location.
    let removable = |r: &RetiredReport| {
        r.kept_reason.is_none()
            && r.locations.iter().all(|l| {
                matches!(
                    l.state,
                    LocationState::Empty
                        | LocationState::Deleted { .. }
                        | LocationState::Shared { .. }
                )
            })
    };
    let mut candidates: Vec<usize> = (0..entries.len())
        .filter(|&i| removable(&entries[i]))
        .collect();
    // The config never loses its last entry: validation rejects a config with
    // none, and an empty one would hide that expiry ever ran. Keep the last
    // candidate in config order.
    let total: usize = config.sources.iter().map(|s| s.subvolumes.len()).sum();
    if candidates.len() == total
        && let Some(last) = candidates.pop()
    {
        entries[last].kept_reason =
            Some("no backups remain — entry kept because it is the last one in the config".into());
    }

    let mut config_error = None;
    if !dry_run && !candidates.is_empty() {
        let mut updated = config.clone();
        let mut touched: Vec<String> = Vec::new();
        for &i in &candidates {
            let report = &entries[i];
            if let Some(source) = updated
                .sources
                .iter_mut()
                .find(|s| s.label == report.source_label)
            {
                source
                    .subvolumes
                    .retain(|e| !(e.name == report.name && e.retired.is_some()));
                touched.push(report.source_label.clone());
                entries[i].removed_from_config = true;
            }
        }
        // A source this run emptied would fail validation; drop it with its
        // last entry. One that was already empty is not this run's to touch.
        updated
            .sources
            .retain(|s| !(s.subvolumes.is_empty() && touched.contains(&s.label)));
        if let Err(e) = updated.save(config_path) {
            config_error = Some(format!("could not write {}: {e}", config_path.display()));
            for report in &mut entries {
                report.removed_from_config = false;
            }
        }
    }
    Ok(ExpireOutcome {
        // Only worth a failure when there was something it stopped.
        clock_error: clock_error.filter(|_| !entries.is_empty()),
        entries,
        config_error,
    })
}

/// The "RETIRED SUBVOLUMES" section of the run report. Empty when no entry
/// is retired, so the section appears only while there is something to say.
pub fn format_expire_report(outcome: &ExpireOutcome, dry_run: bool) -> String {
    if outcome.entries.is_empty() {
        return String::new();
    }
    let plural = |n: usize| if n == 1 { "snapshot" } else { "snapshots" };
    let mut r = String::from("RETIRED SUBVOLUMES\n");
    if let Some(why) = &outcome.clock_error {
        r.push_str(&format!("  NOTHING DELETED: {why}\n"));
    }
    for entry in &outcome.entries {
        r.push_str(&format!(
            "  {}  [source {}, retired {}, series '{}']\n",
            entry.name, entry.source_label, entry.retired, entry.snapshot_name
        ));
        for location in &entry.locations {
            let line = match &location.state {
                LocationState::Unreachable(why) => {
                    format!("not reachable ({why}); nothing changed")
                }
                LocationState::Empty => "no snapshots".to_string(),
                LocationState::Kept {
                    count,
                    expires: Some(date),
                } => {
                    format!(
                        "{count} {} kept, deleted on or after {date}",
                        plural(*count)
                    )
                }
                LocationState::Kept {
                    count,
                    expires: None,
                } => format!(
                    "{count} {} KEPT — no retention window to measure against",
                    plural(*count)
                ),
                LocationState::Deleted { count } if dry_run => {
                    format!("would delete {count} {} (window passed)", plural(*count))
                }
                LocationState::Deleted { count } => {
                    format!("deleted {count} {} (window passed)", plural(*count))
                }
                LocationState::DeleteFailed { deleted, errors } => {
                    format!("DELETE FAILED after {deleted}: {}", errors.join("; "))
                }
                LocationState::Shared { source_label, name } => format!(
                    "series name also used by live entry {name} (source {source_label}) — nothing deleted here"
                ),
                LocationState::RetiredBeforeNewest { count, newest } => format!(
                    "{count} {} KEPT — the retirement date ({}) is earlier than this series' own newest snapshot ({newest}): the clock was wrong at retirement, or the date was edited",
                    plural(*count),
                    entry.retired
                ),
                LocationState::Unrecognised { count, example } => format!(
                    "{count} {} named like this series not recognised as btrbk snapshots (e.g. {example}) — nothing deleted here",
                    if *count == 1 { "entry" } else { "entries" }
                ),
            };
            r.push_str(&format!("    {}: {line}\n", location.place));
        }
        if entry.removed_from_config {
            r.push_str("    no backups remain — entry removed from config\n");
        }
        if let Some(why) = &entry.kept_reason {
            r.push_str(&format!("    {why}\n"));
        }
    }
    if let Some(why) = &outcome.config_error {
        r.push_str(&format!("  CONFIG NOT UPDATED: {why}\n"));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Retention;

    fn r(daily: u32, weekly: u32, monthly: u32, yearly: u32) -> Retention {
        Retention {
            daily,
            weekly,
            monthly,
            yearly,
        }
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
        assert_eq!(
            series_snapshots(&dir, "home-video"),
            ["home-video.20261001T1421"]
        );
        assert!(series_snapshots(&dir, "").is_empty());
    }

    #[test]
    fn series_snapshots_treats_the_name_literally() {
        // '.' and '*' in a snapshot name are characters, not patterns.
        let dir = names(&["a.b.20261001T1421", "aXb.20261001T1421", "a*.20261001T1421"]);
        assert_eq!(series_snapshots(&dir, "a.b"), ["a.b.20261001T1421"]);
        assert_eq!(series_snapshots(&dir, "a*"), ["a*.20261001T1421"]);
    }

    #[test]
    fn each_tier_is_converted_on_its_own() {
        assert_eq!(longest_window_days(&r(0, 1, 0, 0)), Some(7));
        assert_eq!(longest_window_days(&r(0, 0, 1, 0)), Some(31));
        assert_eq!(longest_window_days(&r(0, 0, 0, 2)), Some(732));
        assert_eq!(longest_window_days(&r(400, 0, 0, 1)), Some(400));
        // Overflow saturates upward: a longer window only keeps a backup longer.
        assert_eq!(longest_window_days(&r(0, u32::MAX, 0, 0)), Some(u32::MAX));
    }

    #[test]
    fn windows_cross_month_and_year_boundaries_and_may_be_zero() {
        assert_eq!(expiry_date("2026-12-31", 0).as_deref(), Some("2027-01-01"));
        assert_eq!(expiry_date("2026-02-20", 10).as_deref(), Some("2026-03-03"));
        assert_eq!(is_expired("2026-12-31", 0, "2026-12-31"), Some(false));
        assert_eq!(is_expired("2026-12-31", 0, "2027-01-01"), Some(true));
        assert_eq!(is_expired("2026-02-20", 10, "2026-03-02"), Some(false));
        assert_eq!(is_expired("2026-02-20", 10, "2026-03-03"), Some(true));
        // The day expiry_date names is the first on which is_expired holds.
        assert_eq!(
            is_expired("2026-10-01", 372, &expiry_date("2026-10-01", 372).unwrap()),
            Some(true)
        );
    }

    fn matches(entry: &str) -> bool {
        !series_snapshots(&names(&[entry]), "s").is_empty()
    }

    #[test]
    fn timestamp_shapes_are_matched_exactly() {
        for ok in [
            "s.20261001",
            "s.20261001T1421",
            "s.20261001T142100",
            "s.20261001_1",
            "s.20261001T1421_12",
            "s.20261001T142100_3",
        ] {
            assert!(matches(ok), "{ok} should match");
        }
        for bad in [
            "s.2026100",            // date one digit short
            "s.202610011",          // date one digit long
            "s.2026100a",           // non-digit in date
            "s.20261001T",          // empty time
            "s.20261001T142",       // time 3 digits
            "s.20261001T14210",     // time 5 digits
            "s.20261001T1421000",   // time 7 digits
            "s.20261001T14a1",      // non-digit in time
            "s.20261001Tabcd",      // time all letters
            "s.20261001_",          // empty counter
            "s.20261001_a",         // non-digit counter
            "s.20261001_1_2",       // second underscore
            "s.20261001T1421T1421", // second T
            "s.T1421",              // no date
            "s.",                   // nothing after the dot
            "s.20261001 ",          // trailing space
        ] {
            assert!(!matches(bad), "{bad} should not match");
        }
    }

    #[test]
    fn a_timezone_suffix_is_not_matched_so_such_a_series_is_kept() {
        // btrbk's long-iso format can append an offset. Not matching means
        // "keep", the cautious direction; this pins that behaviour.
        assert!(!matches("s.20261001T142100+0200"));
        assert!(!matches("s.20261001T142100-0500"));
        assert!(!matches("s.20261001T1421+0200"));
    }

    #[test]
    fn series_snapshots_returns_sorted_and_only_the_matching_entries() {
        let dir = names(&["s.20261002", "s.20261001", "t.20261001", "s.20261001T1421"]);
        assert_eq!(
            series_snapshots(&dir, "s"),
            ["s.20261001", "s.20261001T1421", "s.20261002"]
        );
        assert!(series_snapshots(&[], "s").is_empty());
    }

    // ---- shell: what is deleted, where, and when the entry leaves the config ----

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
        let mut c = Config {
            targets: vec![
                target("big", TargetRole::Primary, r(7, 4, 12, 1)),
                target("small", TargetRole::Mirror, r(7, 0, 0, 0)),
            ],
            sources: vec![Source {
                label: "ssd".into(),
                volume: p("vol"),
                subvolumes: vec![
                    SubvolConfig {
                        name: "@srv".into(),
                        ..Default::default()
                    },
                    SubvolConfig {
                        name: "@opt".into(),
                        retired: Some("2026-10-01".into()),
                        ..Default::default()
                    },
                ],
                device: "UUID=abc".into(),
                snapshot_dir: ".btrbk-snapshots".into(),
                target_subdirs: vec!["ssd".into()],
                target_labels: Vec::new(),
            }],
            ..Config::default()
        };
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
            Scripted::deleting(
                paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            )
        }
        fn edit(&self, f: impl FnOnce(&mut Config)) {
            let mut c = Config::load(&self.config_path).unwrap();
            f(&mut c);
            c.save(&self.config_path).unwrap();
        }
        fn saved_names(&self) -> Vec<String> {
            Config::load(&self.config_path)
                .unwrap()
                .sources
                .iter()
                .flat_map(|s| s.subvolumes.iter().map(|e| e.name.clone()))
                .collect()
        }
    }

    fn live(name: &str) -> SubvolConfig {
        SubvolConfig {
            name: name.into(),
            ..Default::default()
        }
    }

    /// A second source holding one entry, sending to every target.
    fn other_source(
        label: &str,
        volume: String,
        snapshot_dir: &str,
        subdir: &str,
        entry: SubvolConfig,
    ) -> Source {
        Source {
            label: label.into(),
            volume,
            subvolumes: vec![entry],
            device: "UUID=def".into(),
            snapshot_dir: snapshot_dir.into(),
            target_subdirs: vec![subdir.into()],
            target_labels: Vec::new(),
        }
    }

    fn mounted(_: &Path) -> bool {
        true
    }

    fn states(out: &ExpireOutcome) -> Vec<LocationState> {
        out.entries[0]
            .locations
            .iter()
            .map(|l| l.state.clone())
            .collect()
    }

    #[test]
    fn nothing_is_deleted_inside_the_window() {
        let rig = rig();
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let big = rig.snap("big/ssd", "opt.20260930T0323");
        let runner = Scripted::new(&[]);
        let out = expire_retired(&rig.config_path, false, "2026-10-08", &runner, &mounted).unwrap();
        assert!(small.exists() && big.exists());
        assert!(runner.calls().is_empty());
        assert!(!out.failed());
        let entry = &out.entries[0];
        assert_eq!(
            (entry.name.as_str(), entry.snapshot_name.as_str()),
            ("@opt", "opt")
        );
        assert_eq!(
            states(&out),
            vec![
                LocationState::Kept {
                    count: 1,
                    expires: Some("2027-10-09".into())
                },
                LocationState::Kept {
                    count: 1,
                    expires: Some("2026-10-09".into())
                },
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
        let out = expire_retired(&rig.config_path, false, "2026-10-09", &runner, &mounted).unwrap();

        assert!(
            !small_a.exists() && !small_b.exists(),
            "past the 7-day window"
        );
        assert!(
            !src.exists(),
            "source snapshots follow the shortest target window"
        );
        assert!(
            big.exists(),
            "the primary keeps it for its own, longer window"
        );
        assert!(
            small_live.exists() && small_other.exists(),
            "other series are never touched"
        );
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
        assert!(rig.saved_names().contains(&"@opt".to_string()));
    }

    #[test]
    fn entry_is_removed_once_no_snapshot_remains_anywhere() {
        let rig = rig();
        let big = rig.snap("big/ssd", "opt.20260930T0323");
        let runner = rig.deleting(&[&big]);
        let out = expire_retired(&rig.config_path, false, "2028-01-01", &runner, &mounted).unwrap();
        assert!(out.entries[0].removed_from_config);
        assert_eq!(rig.saved_names(), ["@srv"]);
    }

    #[test]
    fn entry_is_kept_while_any_location_is_unreachable() {
        let rig = rig();
        let big_mount = rig.dir.path().join("big");
        let only_big_missing: &dyn Fn(&Path) -> bool = &|p| p != big_mount.as_path();
        let out = expire_retired(
            &rig.config_path,
            false,
            "2028-01-01",
            &Scripted::new(&[]),
            only_big_missing,
        )
        .unwrap();
        let states = states(&out);
        assert!(
            matches!(states[0], LocationState::Unreachable(_)),
            "{states:?}"
        );
        assert_eq!(states[1], LocationState::Empty);
        assert!(!out.entries[0].removed_from_config);
        assert!(
            !out.failed(),
            "an absent target is a valid state, not a failure"
        );
        assert!(rig.saved_names().contains(&"@opt".to_string()));
    }

    #[test]
    fn an_unmounted_location_is_never_touched_even_with_snapshots_on_disk() {
        // The mount root, not the snapshot directory, is what is checked: a
        // directory that exists under an unmounted root is the NVMe-fill trap.
        let rig = rig();
        let small = rig.snap("small/ssd", "opt.20200101T0000");
        let src = rig.snap("vol/.btrbk-snapshots", "opt.20200101T0000");
        let small_mount = rig.dir.path().join("small");
        let vol = rig.dir.path().join("vol");
        let checked = std::sync::Mutex::new(Vec::new());
        let mounted_but = |p: &Path| {
            checked.lock().unwrap().push(p.to_path_buf());
            p != small_mount.as_path() && p != vol.as_path()
        };
        let runner = rig.deleting(&[&small, &src]);
        let out =
            expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted_but).unwrap();
        assert!(small.exists() && src.exists());
        assert!(runner.calls().is_empty());
        let s = states(&out);
        assert!(matches!(s[1], LocationState::Unreachable(_)), "{s:?}");
        assert!(matches!(s[2], LocationState::Unreachable(_)), "{s:?}");
        assert_eq!(s[0], LocationState::Empty);
        assert!(!out.entries[0].removed_from_config);
        let checked = checked.lock().unwrap();
        assert!(checked.contains(&small_mount) && checked.contains(&vol));
    }

    #[test]
    fn a_target_with_no_retention_keeps_and_reports() {
        let rig = rig();
        rig.edit(|c| c.targets[1].retention = r(0, 0, 0, 0));
        let small = rig.snap("small/ssd", "opt.20200101T0000");
        let runner = Scripted::new(&[]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(small.exists());
        assert_eq!(
            out.entries[0].locations[1].state,
            LocationState::Kept {
                count: 1,
                expires: None
            }
        );
        assert!(!out.entries[0].removed_from_config);
    }

    #[test]
    fn source_snapshots_are_kept_when_no_target_has_a_window() {
        let rig = rig();
        rig.edit(|c| {
            c.targets[0].retention = r(0, 0, 0, 0);
            c.targets[1].retention = r(0, 0, 0, 0);
        });
        let src = rig.snap("vol/.btrbk-snapshots", "opt.20200101T0000");
        let runner = rig.deleting(&[&src]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(src.exists());
        assert!(runner.calls().is_empty());
        assert_eq!(
            states(&out)[2],
            LocationState::Kept {
                count: 1,
                expires: None
            }
        );
    }

    #[test]
    fn a_target_without_a_window_does_not_stop_the_others_setting_the_source_window() {
        // Source-side window = shortest among targets that HAVE one.
        let rig = rig();
        rig.edit(|c| c.targets[1].retention = r(0, 0, 0, 0));
        let src = rig.snap("vol/.btrbk-snapshots", "opt.20260930T0323");
        let runner = rig.deleting(&[&src]);
        // primary window 372 days: retired 2026-10-01 -> deletable from 2027-10-09.
        let early =
            expire_retired(&rig.config_path, false, "2027-10-08", &runner, &mounted).unwrap();
        assert!(src.exists());
        assert_eq!(
            states(&early)[2],
            LocationState::Kept {
                count: 1,
                expires: Some("2027-10-09".into())
            }
        );
        let late =
            expire_retired(&rig.config_path, false, "2027-10-09", &runner, &mounted).unwrap();
        assert!(!src.exists());
        assert_eq!(states(&late)[2], LocationState::Deleted { count: 1 });
    }

    #[test]
    fn only_the_targets_a_source_sends_to_are_examined() {
        let rig = rig();
        rig.edit(|c| c.sources[0].target_labels = vec!["big".into()]);
        let small = rig.snap("small/ssd", "opt.20200101T0000");
        let runner = rig.deleting(&[&small]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(small.exists(), "small is not a target of this source");
        assert!(runner.calls().is_empty());
        assert_eq!(
            out.entries[0]
                .locations
                .iter()
                .map(|l| l.place.as_str())
                .collect::<Vec<_>>()
                .len(),
            2
        );
        assert!(out.entries[0].locations[0].place.contains("big"));
        assert!(out.entries[0].locations[1].place.starts_with("source "));
        // With only `big` (yearly) sending, the source window is 372 days too.
        assert!(out.entries[0].removed_from_config);
    }

    #[test]
    fn the_target_subdir_is_the_first_one_or_else_the_source_label() {
        let rig = rig();
        rig.edit(|c| c.sources[0].target_subdirs = Vec::new());
        // With no target_subdirs the label `ssd` names the directory.
        let small = rig.snap("small/ssd", "opt.20200101T0000");
        let elsewhere = rig.snap("small/other", "opt.20200101T0000");
        let runner = rig.deleting(&[&small, &elsewhere]);
        expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(!small.exists());
        assert!(elsewhere.exists());

        let rig2 = self::rig();
        rig2.edit(|c| c.sources[0].target_subdirs = vec!["first".into(), "second".into()]);
        let first = rig2.snap("small/first", "opt.20200101T0000");
        let second = rig2.snap("small/second", "opt.20200101T0000");
        let label = rig2.snap("small/ssd", "opt.20200101T0000");
        let runner = rig2.deleting(&[&first, &second, &label]);
        expire_retired(&rig2.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(!first.exists());
        assert!(second.exists() && label.exists());
    }

    #[test]
    fn dry_run_deletes_nothing_and_keeps_the_entry() {
        let rig = rig();
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let runner = rig.deleting(&[&small]);
        let out = expire_retired(&rig.config_path, true, "2026-10-09", &runner, &mounted).unwrap();
        assert!(small.exists());
        assert!(runner.calls().is_empty());
        assert_eq!(
            out.entries[0].locations[1].state,
            LocationState::Deleted { count: 1 }
        );
        assert!(out.entries[0].locations[1].deleted_paths.is_empty());
        assert!(out.deleted_paths().is_empty());
        // Everything is Empty or Deleted, so a real run would remove the entry
        // from the config; a dry run must not, nor say it did.
        assert!(!out.entries[0].removed_from_config);
        assert!(rig.saved_names().contains(&"@opt".to_string()));
        assert!(!out.failed());
        let text = format_expire_report(&out, true);
        assert!(text.contains("would delete 1"), "{text}");
        assert!(!text.contains("removed from config"), "{text}");
    }

    #[test]
    fn a_failed_delete_is_reported_and_marks_the_run_failed() {
        let rig = rig();
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        // No delete is scripted to succeed.
        let out = expire_retired(
            &rig.config_path,
            false,
            "2026-10-09",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert!(small.exists());
        assert!(out.failed());
        assert!(matches!(
            out.entries[0].locations[1].state,
            LocationState::DeleteFailed { deleted: 0, .. }
        ));
        assert!(!out.entries[0].removed_from_config);
    }

    #[test]
    fn a_partial_delete_failure_still_reports_what_was_deleted() {
        let rig = rig();
        let a = rig.snap("small/ssd", "opt.20260929T0323");
        let b = rig.snap("small/ssd", "opt.20260930T0323");
        let runner = rig.deleting(&[&a]);
        let out = expire_retired(&rig.config_path, false, "2026-10-09", &runner, &mounted).unwrap();
        assert!(!a.exists() && b.exists());
        match &out.entries[0].locations[1].state {
            LocationState::DeleteFailed { deleted, errors } => {
                assert_eq!(*deleted, 1);
                assert_eq!(errors.len(), 1);
                assert!(errors[0].contains(&b.display().to_string()), "{errors:?}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(out.deleted_paths(), vec![a]);
        assert!(out.failed());
    }

    #[test]
    fn an_unreadable_retirement_date_expires_nothing() {
        let rig = rig();
        rig.edit(|c| c.sources[0].subvolumes[1].retired = Some("sometime".into()));
        let small = rig.snap("small/ssd", "opt.20200101T0000");
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert!(small.exists());
        assert_eq!(
            out.entries[0].locations[1].state,
            LocationState::Kept {
                count: 1,
                expires: None
            }
        );
    }

    #[test]
    fn an_unreadable_today_expires_nothing() {
        let rig = rig();
        let small = rig.snap("small/ssd", "opt.20200101T0000");
        let runner = rig.deleting(&[&small]);
        let out = expire_retired(&rig.config_path, false, "garbage", &runner, &mounted).unwrap();
        assert!(small.exists());
        assert!(runner.calls().is_empty());
        assert!(matches!(
            out.entries[0].locations[1].state,
            LocationState::Kept { expires: None, .. }
        ));
    }

    #[test]
    fn a_missing_series_directory_is_empty_but_an_unreadable_one_is_not() {
        // A target that never received this source has no directory: Empty.
        let rig = rig();
        std::fs::remove_dir(rig.dir.path().join("small/ssd")).unwrap();
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert_eq!(states(&out)[1], LocationState::Empty);
        assert!(out.entries[0].removed_from_config);

        // A path that cannot be read for any other reason says nothing about
        // what is in it: Unreachable, entry kept.
        let rig = self::rig();
        std::fs::remove_dir(rig.dir.path().join("small/ssd")).unwrap();
        std::fs::write(rig.dir.path().join("small/ssd"), "a file, not a directory").unwrap();
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert!(
            matches!(states(&out)[1], LocationState::Unreachable(_)),
            "{:?}",
            states(&out)
        );
        assert!(!out.entries[0].removed_from_config);
        assert!(!out.failed());
    }

    #[test]
    fn only_the_expired_entry_leaves_the_config() {
        // Two retired entries in one source: removal matches on both name and
        // retirement, never on either alone.
        let rig = rig();
        rig.edit(|c| {
            c.sources[0].subvolumes.push(SubvolConfig {
                name: "@new".into(),
                retired: Some("2028-06-01".into()),
                ..Default::default()
            });
        });
        let kept = rig.snap("big/ssd", "new.20280601T0000");
        let out = expire_retired(
            &rig.config_path,
            false,
            "2028-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert_eq!(out.entries.len(), 2);
        assert!(out.entries[0].removed_from_config, "@opt has nothing left");
        assert!(
            !out.entries[1].removed_from_config,
            "@new still has a snapshot"
        );
        assert!(kept.exists());
        assert_eq!(rig.saved_names(), ["@srv", "@new"]);
    }

    #[test]
    fn a_live_entry_is_never_removed_even_if_its_name_matches_a_retired_one() {
        // The same name in another source on the same volume and snapshot
        // directory: only the retired entry goes, and the live series'
        // snapshots on disk are not touched.
        let rig = rig();
        rig.edit(|c| {
            let volume = c.sources[0].volume.clone();
            c.sources.push(other_source(
                "other",
                volume,
                ".btrbk-snapshots",
                "other",
                live("@opt"),
            ));
        });
        let live_snap = rig.snap("vol/.btrbk-snapshots", "opt.20280101T0000");
        let runner = rig.deleting(&[&live_snap]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(live_snap.exists());
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert_eq!(out.entries.len(), 1, "only the retired entry is examined");
        let saved = Config::load(&rig.config_path).unwrap();
        assert_eq!(saved.sources[0].subvolumes.len(), 1);
        assert_eq!(saved.sources[1].subvolumes[0].name, "@opt");
        assert_eq!(saved.sources[1].subvolumes[0].retired, None);
    }

    #[test]
    fn a_source_left_with_no_entries_is_removed_with_its_last_entry() {
        let rig = rig();
        rig.edit(|c| {
            c.sources.push(Source {
                label: "lonely".into(),
                volume: c.sources[0].volume.clone(),
                subvolumes: vec![SubvolConfig {
                    name: "@gone".into(),
                    retired: Some("2026-01-01".into()),
                    ..Default::default()
                }],
                device: "UUID=def".into(),
                snapshot_dir: ".btrbk-snapshots".into(),
                target_subdirs: vec!["lonely".into()],
                target_labels: Vec::new(),
            });
        });
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert!(out.entries.iter().all(|e| e.removed_from_config));
        let saved = Config::load(&rig.config_path).unwrap();
        assert_eq!(
            saved
                .sources
                .iter()
                .map(|s| s.label.as_str())
                .collect::<Vec<_>>(),
            ["ssd"]
        );
        assert_eq!(rig.saved_names(), ["@srv"]);
        assert!(saved.validate().is_empty(), "{:?}", saved.validate());
    }

    #[test]
    fn a_config_that_cannot_be_saved_keeps_every_entry_and_fails_the_run() {
        let rig = rig();
        // A directory where the atomic write's temp file goes makes saving fail.
        std::fs::create_dir(rig.dir.path().join(".config.toml.tmp")).unwrap();
        let before = std::fs::read_to_string(&rig.config_path).unwrap();
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert!(out.config_error.is_some());
        assert!(out.failed());
        assert!(out.entries.iter().all(|e| !e.removed_from_config));
        assert_eq!(std::fs::read_to_string(&rig.config_path).unwrap(), before);
        let text = format_expire_report(&out, false);
        assert!(text.contains("CONFIG NOT UPDATED"), "{text}");
        assert!(!text.contains("entry removed from config"), "{text}");
    }

    #[test]
    fn a_config_that_cannot_be_loaded_is_an_error_not_an_empty_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let err = expire_retired(
            &dir.path().join("absent.toml"),
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .err()
        .unwrap();
        assert!(err.contains("could not load"), "{err}");
    }

    #[test]
    fn failed_is_false_for_a_clean_outcome_and_for_kept_and_unreachable_locations() {
        let outcome = |state| ExpireOutcome {
            entries: vec![RetiredReport {
                source_label: "s".into(),
                name: "@x".into(),
                snapshot_name: "x".into(),
                retired: "2026-10-01".into(),
                locations: vec![LocationReport {
                    place: "p".into(),
                    state,
                    deleted_paths: Vec::new(),
                }],
                removed_from_config: false,
                kept_reason: None,
            }],
            config_error: None,
            clock_error: None,
        };
        for ok in [
            LocationState::Empty,
            LocationState::Deleted { count: 1 },
            LocationState::Kept {
                count: 1,
                expires: None,
            },
            LocationState::Unreachable("x".into()),
            LocationState::Unrecognised {
                count: 1,
                example: "x".into(),
            },
        ] {
            assert!(!outcome(ok.clone()).failed(), "{ok:?}");
        }
        assert!(
            outcome(LocationState::DeleteFailed {
                deleted: 0,
                errors: vec![]
            })
            .failed()
        );
        let mut c = outcome(LocationState::Empty);
        c.config_error = Some("x".into());
        assert!(c.failed());
    }

    #[test]
    fn report_is_empty_when_nothing_is_retired_and_lists_every_location_otherwise() {
        let rig = rig();
        rig.edit(|c| c.sources[0].subvolumes[1].retired = None);
        let out = expire_retired(
            &rig.config_path,
            false,
            "2026-10-08",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert_eq!(format_expire_report(&out, false), "");

        let rig = self::rig();
        rig.snap("small/ssd", "opt.20260930T0323");
        let out = expire_retired(
            &rig.config_path,
            false,
            "2026-10-08",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        let text = format_expire_report(&out, false);
        assert!(
            text.starts_with(
                "RETIRED SUBVOLUMES\n  @opt  [source ssd, retired 2026-10-01, series 'opt']\n"
            ),
            "{text}"
        );
        assert!(text.contains("no snapshots"), "{text}");
        assert!(
            text.contains("1 snapshot kept, deleted on or after 2026-10-09"),
            "{text}"
        );
    }

    #[test]
    fn report_words_every_state() {
        let loc = |place: &str, state| LocationReport {
            place: place.into(),
            state,
            deleted_paths: Vec::new(),
        };
        let out = ExpireOutcome {
            entries: vec![RetiredReport {
                source_label: "ssd".into(),
                name: "@opt".into(),
                snapshot_name: "opt".into(),
                retired: "2026-10-01".into(),
                locations: vec![
                    loc("a", LocationState::Unreachable("gone".into())),
                    loc("b", LocationState::Empty),
                    loc(
                        "c",
                        LocationState::Kept {
                            count: 2,
                            expires: Some("2027-01-01".into()),
                        },
                    ),
                    loc(
                        "d",
                        LocationState::Kept {
                            count: 1,
                            expires: None,
                        },
                    ),
                    loc("e", LocationState::Deleted { count: 2 }),
                    loc(
                        "f",
                        LocationState::DeleteFailed {
                            deleted: 1,
                            errors: vec!["x".into(), "y".into()],
                        },
                    ),
                ],
                removed_from_config: true,
                kept_reason: None,
            }],
            config_error: Some("disk full".into()),
            clock_error: None,
        };
        let text = format_expire_report(&out, false);
        for line in [
            "    a: not reachable (gone); nothing changed\n",
            "    b: no snapshots\n",
            "    c: 2 snapshots kept, deleted on or after 2027-01-01\n",
            "    d: 1 snapshot KEPT — no retention window to measure against\n",
            "    e: deleted 2 snapshots (window passed)\n",
            "    f: DELETE FAILED after 1: x; y\n",
            "    no backups remain — entry removed from config\n",
            "  CONFIG NOT UPDATED: disk full\n",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
        let dry = format_expire_report(&out, true);
        assert!(
            dry.contains("    e: would delete 2 snapshots (window passed)\n"),
            "{dry}"
        );
        assert!(!dry.contains("deleted 2"), "{dry}");
    }

    // ---- ruling 14: unrecognised entries block "empty" ----

    #[test]
    fn unrecognised_entries_are_those_that_look_like_a_snapshot_of_the_series_but_are_not() {
        let dir = names(&[
            "home.20261001T142100+0200",
            "home.20261001T1421.partial",
            "home.20261001T1421",
            "home-video.20261001T1421",
            "home.tmp",
            "home.2026",
            "home.b.20261001T1421",
            "xhome.20261001T1421",
            "home",
        ]);
        assert_eq!(
            unrecognised_series_entries(&dir, "home"),
            ["home.20261001T1421.partial", "home.20261001T142100+0200"]
        );
        // `home.b` is its own series; its entry is recognised there.
        assert!(unrecognised_series_entries(&dir, "home.b").is_empty());
        assert!(unrecognised_series_entries(&dir, "").is_empty());
        assert!(unrecognised_series_entries(&[], "home").is_empty());
        // Exactly 8 digits is the threshold.
        let edge = names(&["s.1234567", "s.12345678", "s.12345678x", "s.123456789"]);
        assert_eq!(
            unrecognised_series_entries(&edge, "s"),
            ["s.123456789", "s.12345678x"]
        );
    }

    #[test]
    fn a_location_with_only_an_unrecognised_entry_is_not_deleted_and_not_gone() {
        let rig = rig();
        let odd = rig.snap("small/ssd", "opt.20200101T000000+0200");
        let runner = rig.deleting(&[&odd]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(odd.exists());
        assert!(runner.calls().is_empty());
        assert_eq!(
            states(&out)[1],
            LocationState::Unrecognised {
                count: 1,
                example: "opt.20200101T000000+0200".into()
            }
        );
        assert!(!out.entries[0].removed_from_config);
        assert!(!out.failed());
        assert!(rig.saved_names().contains(&"@opt".to_string()));
    }

    #[test]
    fn recognised_snapshots_beside_an_unrecognised_one_are_not_deleted_either() {
        let rig = rig();
        let good = rig.snap("small/ssd", "opt.20200101T0000");
        let odd = rig.snap("small/ssd", "opt.20200101T0000.partial");
        let runner = rig.deleting(&[&good, &odd]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(good.exists() && odd.exists());
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert_eq!(
            states(&out)[1],
            LocationState::Unrecognised {
                count: 1,
                example: "opt.20200101T0000.partial".into()
            }
        );
        assert!(!out.entries[0].removed_from_config);
    }

    #[test]
    fn the_example_is_the_first_unrecognised_entry_in_sorted_order() {
        let rig = rig();
        rig.snap("small/ssd", "opt.20200101zzz");
        rig.snap("small/ssd", "opt.20200101aaa");
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert_eq!(
            states(&out)[1],
            LocationState::Unrecognised {
                count: 2,
                example: "opt.20200101aaa".into()
            }
        );
    }

    #[test]
    fn an_unrecognised_entry_on_the_source_side_blocks_there_too() {
        let rig = rig();
        let odd = rig.snap("vol/.btrbk-snapshots", "opt.20200101T0000_x");
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert!(odd.exists());
        assert!(matches!(
            states(&out)[2],
            LocationState::Unrecognised { count: 1, .. }
        ));
        assert!(!out.entries[0].removed_from_config);
    }

    #[test]
    fn report_line_for_unrecognised_entries_singular_and_plural() {
        let one = ExpireOutcome {
            entries: vec![RetiredReport {
                source_label: "ssd".into(),
                name: "@opt".into(),
                snapshot_name: "opt".into(),
                retired: "2026-10-01".into(),
                locations: vec![
                    LocationReport {
                        place: "target big".into(),
                        state: LocationState::Unrecognised {
                            count: 1,
                            example: "opt.2026100x".into(),
                        },
                        deleted_paths: Vec::new(),
                    },
                    LocationReport {
                        place: "target small".into(),
                        state: LocationState::Unrecognised {
                            count: 3,
                            example: "opt.20261001T1421.partial".into(),
                        },
                        deleted_paths: Vec::new(),
                    },
                ],
                removed_from_config: false,
                kept_reason: None,
            }],
            config_error: None,
            clock_error: None,
        };
        let text = format_expire_report(&one, false);
        assert!(
            text.contains(
                "    target big: 1 entry named like this series not recognised as btrbk snapshots (e.g. opt.2026100x) — nothing deleted here\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "    target small: 3 entries named like this series not recognised as btrbk snapshots (e.g. opt.20261001T1421.partial) — nothing deleted here\n"
            ),
            "{text}"
        );
    }

    // ---- fix round 1, finding 1: a live series sharing the name ----

    fn shared(label: &str, name: &str) -> LocationState {
        LocationState::Shared {
            source_label: label.into(),
            name: name.into(),
        }
    }

    #[test]
    fn a_live_series_sharing_the_source_directory_is_never_deleted() {
        let rig = rig();
        rig.edit(|c| {
            let volume = c.sources[0].volume.clone();
            c.sources.push(other_source(
                "other",
                volume,
                ".btrbk-snapshots",
                "other",
                live("@opt"),
            ));
        });
        let src = rig.snap("vol/.btrbk-snapshots", "opt.20260930T0323");
        let runner = rig.deleting(&[&src]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(src.exists(), "the live series' send parent");
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert_eq!(states(&out)[2], shared("other", "@opt"));
        assert!(!out.failed());
    }

    #[test]
    fn a_live_series_sharing_a_target_directory_by_explicit_snapshot_name_is_never_deleted() {
        let rig = rig();
        let vol2 = rig.dir.path().join("vol2").to_string_lossy().into_owned();
        rig.edit(|c| {
            c.sources.push(other_source(
                "other",
                vol2,
                ".btrbk-snapshots",
                "ssd",
                SubvolConfig {
                    name: "@live".into(),
                    snapshot_name: Some("opt".into()),
                    ..Default::default()
                },
            ));
        });
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let big = rig.snap("big/ssd", "opt.20260930T0323");
        let runner = rig.deleting(&[&small, &big]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(small.exists() && big.exists());
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert_eq!(
            states(&out),
            vec![
                shared("other", "@live"),
                shared("other", "@live"),
                LocationState::Empty
            ]
        );
    }

    #[test]
    fn a_shared_source_directory_does_not_stop_the_unshared_targets_expiring() {
        let rig = rig();
        rig.edit(|c| {
            let volume = c.sources[0].volume.clone();
            c.sources.push(other_source(
                "other",
                volume,
                ".btrbk-snapshots",
                "other",
                live("@opt"),
            ));
        });
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let big = rig.snap("big/ssd", "opt.20260930T0323");
        let src = rig.snap("vol/.btrbk-snapshots", "opt.20260930T0323");
        let runner = rig.deleting(&[&small, &big, &src]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(!small.exists() && !big.exists());
        assert!(src.exists());
        assert_eq!(
            states(&out),
            vec![
                LocationState::Deleted { count: 1 },
                LocationState::Deleted { count: 1 },
                shared("other", "@opt")
            ]
        );
        assert!(out.entries[0].removed_from_config);
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn a_same_name_live_entry_elsewhere_does_not_block_normal_expiry() {
        // Different volume, different target subdirectory: nothing is shared.
        let rig = rig();
        let vol2 = rig.dir.path().join("vol2").to_string_lossy().into_owned();
        rig.edit(|c| {
            c.sources.push(other_source(
                "other",
                vol2,
                ".btrbk-snapshots",
                "other",
                live("@opt"),
            ));
        });
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let big = rig.snap("big/ssd", "opt.20260930T0323");
        let src = rig.snap("vol/.btrbk-snapshots", "opt.20260930T0323");
        let runner = rig.deleting(&[&small, &big, &src]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(!small.exists() && !big.exists() && !src.exists());
        assert!(out.entries[0].removed_from_config);
        assert_eq!(runner.calls().len(), 3);
    }

    #[test]
    fn the_same_volume_with_a_different_snapshot_dir_is_not_shared() {
        let rig = rig();
        rig.edit(|c| {
            let volume = c.sources[0].volume.clone();
            c.sources.push(other_source(
                "other",
                volume,
                ".other-snaps",
                "other",
                live("@opt"),
            ));
        });
        let src = rig.snap("vol/.btrbk-snapshots", "opt.20260930T0323");
        let runner = rig.deleting(&[&src]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(!src.exists());
        assert_eq!(states(&out)[2], LocationState::Deleted { count: 1 });
    }

    #[test]
    fn a_live_entry_whose_source_sends_to_other_targets_does_not_share_this_one() {
        // Same name, same subdirectory string, but it only sends to `big`:
        // `small/ssd` is nobody else's.
        let rig = rig();
        let vol2 = rig.dir.path().join("vol2").to_string_lossy().into_owned();
        rig.edit(|c| {
            let mut other = other_source("other", vol2, ".btrbk-snapshots", "ssd", live("@opt"));
            other.target_labels = vec!["big".into()];
            c.sources.push(other);
        });
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let big = rig.snap("big/ssd", "opt.20260930T0323");
        let runner = rig.deleting(&[&small, &big]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(big.exists(), "shared with the live entry");
        assert!(
            !small.exists(),
            "not shared: the live source never sends here"
        );
        assert_eq!(states(&out)[0], shared("other", "@opt"));
        assert_eq!(states(&out)[1], LocationState::Deleted { count: 1 });
    }

    #[test]
    fn report_line_for_a_shared_location() {
        let out = ExpireOutcome {
            entries: vec![RetiredReport {
                source_label: "ssd".into(),
                name: "@opt".into(),
                snapshot_name: "opt".into(),
                retired: "2026-10-01".into(),
                locations: vec![LocationReport {
                    place: "target big".into(),
                    state: shared("other", "@opt"),
                    deleted_paths: Vec::new(),
                }],
                removed_from_config: false,
                kept_reason: None,
            }],
            config_error: None,
            clock_error: None,
        };
        assert!(
            format_expire_report(&out, false).contains(
                "    target big: series name also used by live entry @opt (source other) — nothing deleted here\n"
            ),
        );
    }

    // ---- fix round 1, finding 2 and rulings 17 and 18 ----

    #[test]
    fn a_directory_entry_that_is_not_utf8_makes_the_location_unreachable() {
        use std::os::unix::ffi::OsStrExt;
        let rig = rig();
        let old = rig.snap("small/ssd", "opt.20200101T0000");
        let odd = rig
            .dir
            .path()
            .join("small/ssd")
            .join(std::ffi::OsStr::from_bytes(b"opt.20200101T0000\xff"));
        std::fs::write(&odd, "x").unwrap();
        let runner = rig.deleting(&[&old]);
        let out = expire_retired(&rig.config_path, false, "2030-01-01", &runner, &mounted).unwrap();
        assert!(old.exists() && odd.exists());
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        match &states(&out)[1] {
            LocationState::Unreachable(why) => {
                assert!(why.contains("not valid UTF-8"), "{why}");
                assert!(why.contains("opt.20200101T0000\u{FFFD}"), "{why}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!out.entries[0].removed_from_config);
        assert!(!out.failed());
        assert!(rig.saved_names().contains(&"@opt".to_string()));
    }

    #[test]
    fn collect_names_fails_on_any_read_error_or_non_utf8_name() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let dir = Path::new("/d");
        let ok: Vec<std::io::Result<OsString>> = vec![Ok("a".into()), Ok("b".into())];
        assert_eq!(collect_names(ok.into_iter(), dir).unwrap(), ["a", "b"]);

        let broken: Vec<std::io::Result<OsString>> = vec![
            Ok("a".into()),
            Err(std::io::Error::other("disk went away")),
            Ok("b".into()),
        ];
        let why = collect_names(broken.into_iter(), dir).unwrap_err();
        assert!(
            why.contains("/d") && why.contains("disk went away"),
            "{why}"
        );

        let odd: Vec<std::io::Result<OsString>> = vec![Ok(OsString::from_vec(b"x\xfey".to_vec()))];
        let why = collect_names(odd.into_iter(), dir).unwrap_err();
        assert!(
            why.contains("not valid UTF-8") && why.contains("x\u{FFFD}y"),
            "{why}"
        );
        assert!(collect_names(std::iter::empty(), dir).unwrap().is_empty());
    }

    /// The rig with `@srv` removed, so `@opt` is the only entry in the config.
    fn rig_with_only_the_retired_entry() -> Rig {
        let rig = rig();
        rig.edit(|c| drop(c.sources[0].subvolumes.remove(0)));
        rig
    }

    const LAST_ONE: &str =
        "    no backups remain — entry kept because it is the last one in the config\n";

    #[test]
    fn the_last_entry_of_the_config_is_never_removed() {
        let rig = rig_with_only_the_retired_entry();
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        let entry = &out.entries[0];
        assert!(!entry.removed_from_config);
        assert!(entry.kept_reason.is_some());
        assert!(!out.failed());
        let saved = Config::load(&rig.config_path).unwrap();
        assert_eq!(saved.sources.len(), 1);
        assert_eq!(rig.saved_names(), ["@opt"]);
        assert!(
            saved
                .validate()
                .iter()
                .all(|e| !e.contains("No backup sources"))
        );
        let text = format_expire_report(&out, false);
        assert!(text.contains(LAST_ONE), "{text}");
        assert!(!text.contains("entry removed from config"), "{text}");
    }

    #[test]
    fn with_two_expired_entries_in_one_source_exactly_the_last_in_config_order_is_kept() {
        let rig = rig_with_only_the_retired_entry();
        rig.edit(|c| {
            c.sources[0].subvolumes.push(SubvolConfig {
                name: "@opt2".into(),
                retired: Some("2026-10-01".into()),
                ..Default::default()
            })
        });
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert_eq!(out.entries.len(), 2);
        assert!(out.entries[0].removed_from_config);
        assert!(out.entries[0].kept_reason.is_none());
        assert!(!out.entries[1].removed_from_config);
        assert!(out.entries[1].kept_reason.is_some());
        assert_eq!(rig.saved_names(), ["@opt2"]);
    }

    #[test]
    fn a_live_entry_elsewhere_means_the_retired_one_is_not_the_last() {
        let rig = rig_with_only_the_retired_entry();
        rig.edit(|c| {
            let volume = c.sources[0].volume.clone();
            c.sources
                .push(other_source("other", volume, ".x", "other", live("@keep")));
        });
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert!(out.entries[0].removed_from_config);
        assert_eq!(rig.saved_names(), ["@keep"]);
    }

    #[test]
    fn a_source_that_was_already_empty_is_left_alone() {
        let rig = rig();
        rig.edit(|c| {
            c.sources.push(Source {
                label: "empty".into(),
                subvolumes: Vec::new(),
                ..other_source(
                    "empty",
                    c.sources[0].volume.clone(),
                    ".y",
                    "empty",
                    live("@x"),
                )
            })
        });
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert!(out.entries[0].removed_from_config);
        let saved = Config::load(&rig.config_path).unwrap();
        assert_eq!(
            saved
                .sources
                .iter()
                .map(|s| s.label.as_str())
                .collect::<Vec<_>>(),
            ["ssd", "empty"]
        );
    }

    #[test]
    fn an_entry_whose_source_sends_to_no_configured_target_is_kept() {
        let rig = rig();
        rig.edit(|c| c.sources[0].target_labels = vec!["no-such-target".into()]);
        let out = expire_retired(
            &rig.config_path,
            false,
            "2030-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        let entry = &out.entries[0];
        assert_eq!(entry.locations.len(), 1, "only the source side");
        assert_eq!(entry.locations[0].state, LocationState::Empty);
        assert!(!entry.removed_from_config);
        assert!(!out.failed());
        assert!(rig.saved_names().contains(&"@opt".to_string()));
        let text = format_expire_report(&out, false);
        assert!(
            text.contains("    no configured target receives source ssd — entry kept\n"),
            "{text}"
        );
    }

    // --- a wrong clock must cost a late deletion, never an early one ---

    #[test]
    fn a_series_whose_newest_snapshot_postdates_its_retirement_is_kept() {
        // Retired 2026-10-01, but a snapshot dated 2026-10-03 exists: the
        // retirement date is wrong (clock, or hand edit). Long past the window.
        let rig = rig();
        let old = rig.snap("small/ssd", "opt.20260930T0323");
        let new = rig.snap("small/ssd", "opt.20261003T0323");
        let runner = rig.deleting(&[&old, &new]);
        let out = expire_retired(&rig.config_path, false, "2027-12-01", &runner, &mounted).unwrap();
        assert!(old.exists() && new.exists());
        assert!(
            !runner.calls().iter().any(|c| c.contains("small/ssd")),
            "{:?}",
            runner.calls()
        );
        assert_eq!(
            out.entries[0].locations[1].state,
            LocationState::RetiredBeforeNewest {
                count: 2,
                newest: "2026-10-03".into()
            }
        );
        assert!(!out.entries[0].removed_from_config);
        assert!(rig.saved_names().contains(&"@opt".to_string()));
        let text = format_expire_report(&out, false);
        assert!(
            text.contains(
                "    target small: 2 snapshots KEPT — the retirement date (2026-10-01) is earlier than this series' own newest snapshot (2026-10-03): the clock was wrong at retirement, or the date was edited\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_snapshot_dated_the_retirement_day_or_the_day_after_does_not_block_expiry() {
        // Snapshot names carry local time, retirement dates are UTC: one day
        // of difference is ordinary and must not keep anything.
        for stamp in ["20261001T2350", "20261002T0010"] {
            let rig = rig();
            let snap = rig.snap("small/ssd", &format!("opt.{stamp}"));
            let runner = rig.deleting(&[&snap]);
            let out =
                expire_retired(&rig.config_path, false, "2026-10-20", &runner, &mounted).unwrap();
            assert!(!snap.exists(), "{stamp}: must be deleted");
            assert_eq!(
                out.entries[0].locations[1].state,
                LocationState::Deleted { count: 1 },
                "{stamp}"
            );
        }
    }

    #[test]
    fn a_clock_before_2026_deletes_nothing_and_fails_loudly() {
        let rig = rig();
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let runner = rig.deleting(&[&small]);
        // A clock that wrapped to the epoch; and one that is merely early.
        for today in ["1970-01-01", "2025-06-01"] {
            let out = expire_retired(&rig.config_path, false, today, &runner, &mounted).unwrap();
            assert!(small.exists(), "{today}");
            assert!(out.failed(), "{today}");
            assert!(out.deleted_paths().is_empty());
            let text = format_expire_report(&out, false);
            assert!(text.contains("  NOTHING DELETED: "), "{text}");
            assert!(text.contains(today), "{text}");
        }
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        // Control: with a real date the same snapshot goes.
        let out = expire_retired(&rig.config_path, false, "2026-10-20", &runner, &mounted).unwrap();
        assert!(!small.exists() && !out.failed());
    }

    #[test]
    fn an_untrusted_clock_is_not_a_failure_when_nothing_is_retired() {
        let rig = rig();
        rig.edit(|c| c.sources[0].subvolumes[1].retired = None);
        let out = expire_retired(
            &rig.config_path,
            false,
            "1970-01-01",
            &Scripted::new(&[]),
            &mounted,
        )
        .unwrap();
        assert!(!out.failed());
        assert_eq!(format_expire_report(&out, false), "");
    }

    // --- expiry never outruns a sync that could not look ---

    #[test]
    fn a_retired_subvolume_that_exists_again_is_kept_everywhere() {
        let rig = rig();
        // The subvolume is back on the (mounted) source volume; this run's
        // sync has not revived it (it failed, or has not run).
        std::fs::create_dir_all(rig.dir.path().join("vol/@opt")).unwrap();
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let source = rig.snap("vol/.btrbk-snapshots", "opt.20260930T0323");
        let runner = rig.deleting(&[&small, &source]);
        let out = expire_retired(&rig.config_path, false, "2026-12-01", &runner, &mounted).unwrap();
        assert!(small.exists() && source.exists());
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        let entry = &out.entries[0];
        assert!(entry.locations.is_empty(), "{:?}", entry.locations);
        assert!(!entry.removed_from_config);
        assert!(rig.saved_names().contains(&"@opt".to_string()));
        assert!(
            !out.failed(),
            "keeping is not a failure; the sync failure is"
        );
        let text = format_expire_report(&out, false);
        assert!(
            text.contains(
                "    subvolume exists again; waiting for sync to revive it — nothing deleted\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_path_under_an_unmounted_volume_is_not_taken_for_a_returned_subvolume() {
        // An unmounted volume's directory says nothing about the filesystem
        // that belongs there, so it is not evidence either way; expiry goes
        // on as before (and the source side is unreachable).
        let rig = rig();
        std::fs::create_dir_all(rig.dir.path().join("vol/@opt")).unwrap();
        let small = rig.snap("small/ssd", "opt.20260930T0323");
        let runner = rig.deleting(&[&small]);
        let vol = rig.dir.path().join("vol");
        let not_vol = move |p: &Path| p != vol;
        let out = expire_retired(&rig.config_path, false, "2026-12-01", &runner, &not_vol).unwrap();
        assert!(!small.exists(), "the target-side series still expires");
        assert_eq!(
            out.entries[0].locations[1].state,
            LocationState::Deleted { count: 1 }
        );
    }
}
