//! Subvolume coverage: what the configuration schedules to be backed up,
//! compared run over run (bd DAS-Backup-Manager-sp78.1).
//!
//! A subvolume can stop being backed up while it still exists without any
//! step failing: its entry is removed and an exclusion then skips it, its
//! `[[source]]` is removed, `target_labels` narrows, a `[[target]]` goes.
//! Each recorded check (one per real backup run, inside the subvolume sync)
//! keeps the set of `(volume, subvolume, target)` pairs the configuration
//! schedules; the next check compares, and a pair scheduled before, still on
//! disk and not scheduled now is a WARN naming the rule that dropped it. It
//! repeats on every run until the pair is restored, gone from disk or
//! accepted (`btrdasd subvol coverage accept`).
//!
//! Coverage compares INTENT — what `config.toml` schedules — never delivery:
//! an absent target, a one-source run or a dry run
//! leaves intent unchanged and cannot raise a false drop. Delivery failures
//! are FAIL rows elsewhere. The check never fails a sync, a backup or an
//! exit code.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::adopt::{SkipReason, SyncPlan, TOP_LEVEL, VolumeListing, display_name};
use crate::config::Config;
use crate::db::{Baseline, CoverageRow, CoverageState, Database};

/// What the coverage row says when no check ran at all, because the
/// subvolume sync that carries it could not run. `backup-run.sh` holds the
/// same text, and a test reads the script to keep the two equal.
pub const NOT_RUN_DETAIL: &str = "not checked: subvolume sync could not run";

/// The first line of the report section, which `backup-run.sh` reads its
/// coverage row from.
pub const SECTION_PREFIX: &str = "SUBVOLUME COVERAGE: ";

/// One scheduled backup: a subvolume of a volume, sent to a target. The
/// source label and the snapshot name are not part of it, so renaming either
/// changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pair {
    pub volume: String,
    /// `.` is the volume's top level.
    pub subvolume: String,
    /// The target's label.
    pub target: String,
}

/// Every pair `cfg` schedules, each with the label of the source that holds
/// it: per source, per live (not retired) entry — `after_index` ones
/// included, since they are rendered and sent — per
/// target the source's volume block sends to. A top level in a source that
/// reaches a mirror is left out: the renderer refuses it.
pub fn intended(cfg: &Config) -> BTreeMap<Pair, String> {
    let mut out = BTreeMap::new();
    for source in &cfg.sources {
        let targets = crate::btrbk_conf::source_targets(cfg, source);
        let reaches_mirror = !cfg.mirrors_reached(source).is_empty();
        for entry in source.subvolumes.iter().filter(|e| e.retired.is_none()) {
            if entry.name == TOP_LEVEL && reaches_mirror {
                continue;
            }
            for target in &targets {
                out.entry(Pair {
                    volume: source.volume.clone(),
                    subvolume: entry.name.clone(),
                    target: target.label.clone(),
                })
                .or_insert_with(|| source.label.clone());
            }
        }
    }
    out
}

/// Whether this check was recorded, and if not, why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recording {
    /// Written: the baseline now holds what this check found.
    Recorded,
    /// A dry run, which never records.
    DryRun,
    /// A sync that is not a backup run (by hand, `backup snapshot|send`).
    NotAsked,
    /// The write was asked for and failed; the baseline is unchanged.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageKind {
    /// No earlier record to compare with.
    NoBaseline,
    /// Compared with the record of an earlier backup run.
    Compared,
    /// The check could not be made, and why.
    NotChecked(String),
}

/// Pairs dropped for the same reason since the same day: one block in the
/// report, its targets merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropBlock {
    pub volume: String,
    pub subvolume: String,
    pub targets: Vec<String>,
    pub why: String,
    pub since: String,
    /// The recorded checks that found it dropped, this one included when it
    /// was recorded.
    pub runs: u32,
}

/// Pairs scheduled again after being dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    pub volume: String,
    pub subvolume: String,
    pub targets: Vec<String>,
    pub since: String,
}

/// A target whose label changed while its drive (mount UUID or serial) did
/// not: its pairs carried over to the new label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenamedTarget {
    pub from: String,
    pub to: String,
    pub pairs: usize,
}

/// What one coverage check found. No `Default`: a default would read as a
/// clean check (mutation-testing.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageOutcome {
    pub kind: CoverageKind,
    /// How many pairs the configuration schedules now.
    pub pairs: usize,
    /// The record compared with.
    pub compared_with: Option<CoverageState>,
    pub drops: Vec<DropBlock>,
    pub restored: Vec<Restored>,
    pub renamed_targets: Vec<RenamedTarget>,
    /// Dropped or covered pairs whose subvolume is gone from a volume that
    /// was read: retirement, not a warning.
    pub gone: usize,
    /// Pairs scheduled now that the record did not hold.
    pub new_pairs: usize,
    pub recording: Recording,
    /// The database the check used, when it is not
    /// [`DEFAULT_DB_PATH`](crate::db::DEFAULT_DB_PATH): the accept line names
    /// it, so pasting the line acts on the database that warned.
    pub accept_db: Option<String>,
}

/// `1 subvolume/target pair`, `N subvolume/target pairs`.
fn pairs_text(n: usize) -> String {
    if n == 1 {
        "1 subvolume/target pair".to_string()
    } else {
        format!("{n} subvolume/target pairs")
    }
}

/// `text` on one line: a report row is read by the line, so an error that
/// spans several (a TOML parse error does) must not break the row.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `s` quoted for a POSIX shell, so the accept line can be pasted as it is.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl CoverageOutcome {
    /// A check that could not be made.
    pub fn not_checked(why: String) -> Self {
        Self {
            kind: CoverageKind::NotChecked(one_line(&why)),
            pairs: 0,
            compared_with: None,
            drops: Vec::new(),
            restored: Vec::new(),
            renamed_targets: Vec::new(),
            gone: 0,
            new_pairs: 0,
            recording: Recording::NotAsked,
            accept_db: None,
        }
    }

    /// How many pairs are reported dropped (not blocks).
    pub fn dropped_pairs(&self) -> usize {
        self.drops.iter().map(|b| b.targets.len()).sum()
    }

    /// A WARN: the check was not made, something is no longer backed up, or
    /// the record could not be written.
    pub fn has_warnings(&self) -> bool {
        matches!(self.kind, CoverageKind::NotChecked(_))
            || !self.drops.is_empty()
            || matches!(self.recording, Recording::Failed(_))
    }

    pub fn status(&self) -> &'static str {
        if self.has_warnings() { "WARN" } else { "OK" }
    }

    /// The text inside the row's parentheses.
    pub fn detail(&self) -> String {
        let base = match &self.kind {
            CoverageKind::NotChecked(why) => return format!("not checked: {why}"),
            CoverageKind::NoBaseline => format!("no earlier record: {}", pairs_text(self.pairs)),
            CoverageKind::Compared if self.drops.is_empty() => {
                format!("{}, none dropped", pairs_text(self.pairs))
            }
            CoverageKind::Compared => {
                format!("{} no longer backed up", pairs_text(self.dropped_pairs()))
            }
        };
        let suffix = match (&self.recording, &self.kind) {
            (Recording::Recorded, CoverageKind::NoBaseline) => {
                "; recorded — drops are reported from the next run".to_string()
            }
            (Recording::Recorded, _) => String::new(),
            (Recording::DryRun, _) => " — dry run, not recorded".to_string(),
            (Recording::NotAsked, _) => " — not recorded (only a backup run records)".to_string(),
            (Recording::Failed(e), _) => format!("; not recorded: {}", one_line(e)),
        };
        format!("{base}{suffix}")
    }

    /// `STATUS  (detail)` — two spaces, as every other operations row in both
    /// reports and in `backup-run.sh`: the report's `Subvolume coverage` row.
    pub fn row(&self) -> String {
        format!("{}  ({})", self.status(), self.detail())
    }

    /// The `SUBVOLUME COVERAGE` section of the run report. Its first line is
    /// the row, which `backup-run.sh` reads.
    pub fn section(&self) -> String {
        let mut r = format!("{SECTION_PREFIX}{}\n", self.row());
        if let Some(state) = &self.compared_with {
            let checks = if state.checks == 1 {
                "1 recorded check".to_string()
            } else {
                format!("{} recorded checks", state.checks)
            };
            r.push_str(&format!(
                "  Compared with the record of {} ({checks} since {})\n",
                state.last_recorded, state.first_recorded
            ));
        }
        if !self.drops.is_empty() {
            r.push_str("  NO LONGER BACKED UP:\n");
            let db = self
                .accept_db
                .as_deref()
                .map_or(String::new(), |d| format!(" --db {}", shell_quote(d)));
            for b in &self.drops {
                let runs = match b.runs {
                    0 => "not yet in a recorded run".to_string(),
                    1 => "1 recorded run".to_string(),
                    n => format!("{n} recorded runs"),
                };
                // Accept takes a drop the record holds; one only this
                // report has seen (a dry run, a compare-only sync, a failed
                // write) is recorded by the next backup run.
                let accept = if b.runs == 0 {
                    "once a backup run has recorded this drop (this report records nothing)"
                        .to_string()
                } else {
                    format!(
                        "btrdasd subvol coverage accept{db} {} {}",
                        shell_quote(&b.volume),
                        shell_quote(&b.subvolume)
                    )
                };
                r.push_str(&format!(
                    "    {}  {}  -> {}\n      why:    {}\n      since:  {}, {runs}\n      \
                     accept: {accept}\n",
                    b.volume,
                    display_name(&b.subvolume),
                    b.targets.join(", "),
                    b.why,
                    b.since,
                ));
            }
            r.push_str(
                "  Not intended? Fix what the \"why\" line names; the next backup run reports it \
                 RESTORED.\n",
            );
            if self.drops.iter().any(|b| b.runs > 0) {
                r.push_str(
                    "  Intended?     Run its accept line (add --target LABEL for one target, or \
                     use --all for every pair above).\n",
                );
            }
        }
        for x in &self.restored {
            r.push_str(&format!(
                "  RESTORED:  {}  {}  -> {}  (no longer backed up since {})\n",
                x.volume,
                display_name(&x.subvolume),
                x.targets.join(", "),
                x.since
            ));
        }
        for t in &self.renamed_targets {
            r.push_str(&format!(
                "  TARGET RENAMED: '{}' is now '{}' (same mount UUID or serial): {} carried over\n",
                t.from,
                t.to,
                pairs_text(t.pairs)
            ));
        }
        if self.gone > 0 {
            r.push_str(&format!(
                "  Gone from disk (retired by sync, not a warning): {}\n",
                pairs_text(self.gone)
            ));
        }
        r
    }
}

/// The section for `outcome`; none at all (the sync did not run) is the
/// cautious WARN, never silence.
pub fn section(outcome: Option<&CoverageOutcome>) -> String {
    match outcome {
        Some(o) => o.section(),
        None => format!("{SECTION_PREFIX}WARN  ({NOT_RUN_DETAIL})\n"),
    }
}

/// The row for `outcome`, the cautious WARN when there is none.
pub fn row_or_not_run(outcome: Option<&CoverageOutcome>) -> String {
    match outcome {
        Some(o) => o.row(),
        None => format!("WARN  ({NOT_RUN_DETAIL})"),
    }
}

/// The line a backup's finish text (the CLI's last line, the GUI's
/// JobFinished summary) carries for the check: `subvolume coverage WARN:
/// <detail>` when the row is a WARN, empty when it is OK — as the recovery-OS
/// line is.
pub fn message_line(outcome: Option<&CoverageOutcome>) -> String {
    if !has_warnings_or_missing(outcome) {
        return String::new();
    }
    let detail = outcome.map_or_else(|| NOT_RUN_DETAIL.to_string(), CoverageOutcome::detail);
    format!("subvolume coverage WARN: {detail}")
}

/// Whether the row for `outcome` is a WARN; none at all is.
pub fn has_warnings_or_missing(outcome: Option<&CoverageOutcome>) -> bool {
    outcome.is_none_or(CoverageOutcome::has_warnings)
}

/// The baseline's pairs with each renamed target's label replaced: an old
/// label no longer configured whose rows all carry one identity, matched by
/// exactly one newly configured target with that identity and claimed by no
/// other old label.
fn carry_over_renames(
    rows: &[CoverageRow],
    now: &Config,
) -> (Vec<CoverageRow>, Vec<RenamedTarget>) {
    let configured: BTreeSet<&str> = now.targets.iter().map(|t| t.label.as_str()).collect();
    let recorded: BTreeSet<&str> = rows.iter().map(|r| r.target.as_str()).collect();
    let mut renames: BTreeMap<&str, &str> = BTreeMap::new();
    for old in recorded.iter().filter(|l| !configured.contains(*l)) {
        let ids: BTreeSet<Option<&String>> = rows
            .iter()
            .filter(|r| r.target == *old)
            .map(|r| r.target_identity.as_ref())
            .collect();
        let [Some(id)] = ids.into_iter().collect::<Vec<_>>()[..] else {
            continue;
        };
        let matching: Vec<&str> = now
            .targets
            .iter()
            .filter(|t| !recorded.contains(t.label.as_str()))
            .filter(|t| t.coverage_identity().as_ref() == Some(id))
            .map(|t| t.label.as_str())
            .collect();
        if let [new] = matching[..] {
            renames.insert(old, new);
        }
    }
    let claimed: Vec<&str> = renames.values().copied().collect();
    renames.retain(|_, new| claimed.iter().filter(|c| *c == new).count() == 1);

    let mut out = rows.to_vec();
    let mut renamed = Vec::new();
    for (old, new) in renames {
        let mut n = 0;
        for r in out.iter_mut().filter(|r| r.target == old) {
            r.target = new.to_string();
            n += 1;
        }
        renamed.push(RenamedTarget {
            from: old.to_string(),
            to: new.to_string(),
            pairs: n,
        });
    }
    (out, renamed)
}

/// A baseline row, placed where its subvolume is now.
struct Followed {
    row: CoverageRow,
    /// The path the row was recorded under, when the subvolume has been
    /// renamed since.
    was: Option<String>,
    /// The row's UUID is the one the listing shows at the row's (new) path.
    confirmed: bool,
}

/// The listing of `volume`, when it was read.
fn read_listing<'a>(listings: &'a [VolumeListing], volume: &str) -> Option<&'a VolumeListing> {
    listings
        .iter()
        .find(|l| l.volume == volume)
        .filter(|l| l.subvolumes.is_ok())
}

/// Where each baseline row's subvolume is now (bd DAS-Backup-Manager-sp78.5).
/// A row holding a subvolume UUID that a read listing of its volume shows at
/// another path moves to that path: the subvolume was renamed, and it is the
/// same pair. Anything less certain stays where it was recorded and is
/// paired by path, as a row with no UUID always was: a volume not read, a
/// listing that gave no UUID for the row's, a UUID no longer there. Rows
/// whose UUID the listing confirms sort first, so when a name is claimed by
/// one subvolume's row and by another's older path-only row, the first keeps
/// it.
fn follow_renames(rows: Vec<CoverageRow>, listings: &[VolumeListing]) -> Vec<Followed> {
    let mut out: Vec<Followed> = rows
        .into_iter()
        .map(|mut row| {
            let listing = read_listing(listings, &row.volume);
            let moved_to = row.subvol_uuid.as_deref().and_then(|u| {
                listing?
                    .uuids
                    .iter()
                    .find(|(_, listed)| listed.as_str() == u)
                    .map(|(path, _)| path.clone())
            });
            let was = moved_to
                .filter(|path| *path != row.subvolume)
                .map(|path| std::mem::replace(&mut row.subvolume, path));
            let confirmed = row.subvol_uuid.is_some()
                && row.subvol_uuid.as_ref() == listing.and_then(|l| l.uuids.get(&row.subvolume));
            Followed {
                row,
                was,
                confirmed,
            }
        })
        .collect();
    out.sort_by_key(|f| !f.confirmed);
    out
}

/// Why `row`, no longer scheduled, is not backed up — or `None` when its
/// subvolume is gone from a volume that was read (retirement, not a drop).
fn why_dropped(
    row: &CoverageRow,
    now: &Config,
    plan: &SyncPlan,
    listings: &[VolumeListing],
) -> Option<String> {
    let (v, s, t) = (&row.volume, &row.subvolume, &row.target);
    if !now.sources.iter().any(|x| &x.volume == v) {
        return Some(format!(
            "no [[source]] names volume {v} any more (last in source '{}'); whether it still \
             exists was not checked",
            row.source_label
        ));
    }
    let unread = match listings.iter().find(|l| &l.volume == v) {
        Some(VolumeListing {
            subvolumes: Ok(names),
            ..
        }) => {
            if s != TOP_LEVEL && !names.contains(s) {
                return None;
            }
            false
        }
        _ => true,
    };
    let on_volume = || now.sources.iter().filter(|x| &x.volume == v);
    let live = on_volume().find(|x| {
        x.subvolumes
            .iter()
            .any(|e| &e.name == s && e.retired.is_none())
    });
    let rule = match live {
        None => {
            let here = |volume: &str, name: &str| volume == v && name == s;
            if let Some(u) = plan.unplaceable.iter().find(|u| here(&u.volume, &u.name)) {
                format!("could not be placed — see COULD NOT BE PLACED: {}", u.why)
            } else if let Some(x) = on_volume().find(|x| x.subvolumes.iter().any(|e| &e.name == s))
            {
                format!(
                    "its entry in source '{}' is retired although it is still on disk — see \
                     SUBVOLUME SYNC",
                    x.label
                )
            } else if let Some(skip) = plan.skipped.iter().find(|k| here(&k.volume, &k.name)) {
                match &skip.reason {
                    SkipReason::Excluded { pattern } => format!(
                        "excluded by pattern '{pattern}' ({}); no config entry names it",
                        now.exclude_pattern_origins(pattern).join(", ")
                    ),
                    SkipReason::SnapshotTree => "inside a snapshot directory (.snapshots or \
                                                 .btrbk-snapshots); no config entry names it"
                        .to_string(),
                }
            } else if plan.adopt.iter().any(|a| here(&a.volume, &a.name)) {
                "its adoption was planned but not written — see SUBVOLUME SYNC".to_string()
            } else if unread {
                return Some(format!(
                    "no config entry names it; volume {v} was not read this run (see VOLUMES NOT \
                     READ), so whether it is excluded or still exists was not checked"
                ));
            } else {
                "no rule found that explains it (report this)".to_string()
            }
        }
        Some(source) => {
            let reached = crate::btrbk_conf::source_targets(now, source);
            if !now.targets.iter().any(|x| &x.label == t) {
                format!("target '{t}' is no longer configured")
            } else if reached.is_empty() {
                format!(
                    "source '{}' sends to no configured target (target_labels = [{}])",
                    source.label,
                    source
                        .target_labels
                        .iter()
                        .map(|l| format!("'{l}'"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else if source.label.ends_with("-adopted") && row.source_label != source.label {
                format!(
                    "its entry left source '{}' and was re-adopted into '{}', which sends to {}",
                    row.source_label,
                    source.label,
                    crate::adopt::scope_of(now, source)
                )
            } else if reached.iter().any(|x| &x.label == t) {
                if s == TOP_LEVEL {
                    format!(
                        "the top level cannot be sent while source '{}' reaches a mirror — see \
                         COULD NOT BE PLACED",
                        source.label
                    )
                } else {
                    "no rule found that explains it (report this)".to_string()
                }
            } else {
                format!(
                    "source '{}' no longer sends to '{t}' (it sends to {})",
                    source.label,
                    crate::adopt::scope_of(now, source)
                )
            }
        }
    };
    Some(if unread {
        format!("{rule} (volume {v} was not read this run)")
    } else {
        rule
    })
}

/// Compare `baseline` with what `now` schedules. Pure: returns the outcome,
/// the rows the baseline would hold after this check, and its state. Only
/// the caller decides whether they are written (`recording` says which).
///
/// `plan` and `listings` are the sync's own, used only to name why a pair is
/// no longer scheduled and to tell a gone subvolume from a dropped one.
pub fn evaluate(
    baseline: Option<&Baseline>,
    now: &Config,
    plan: &SyncPlan,
    listings: &[VolumeListing],
    today: &str,
    recording: Recording,
) -> (CoverageOutcome, Vec<CoverageRow>, CoverageState) {
    let intent = intended(now);
    // The UUID the listing of the pair's volume gives its subvolume now.
    let uuid_now = |volume: &str, subvolume: &str| {
        read_listing(listings, volume).and_then(|l| l.uuids.get(subvolume).cloned())
    };
    let covered = |pair: &Pair, source_label: &str, first_covered: &str, known: Option<&String>| {
        CoverageRow {
            volume: pair.volume.clone(),
            subvolume: pair.subvolume.clone(),
            target: pair.target.clone(),
            target_identity: now
                .targets
                .iter()
                .find(|t| t.label == pair.target)
                .and_then(|t| t.coverage_identity()),
            source_label: source_label.to_string(),
            first_covered: first_covered.to_string(),
            last_covered: today.to_string(),
            dropped_since: None,
            dropped_checks: 0,
            drop_rule: None,
            subvol_uuid: uuid_now(&pair.volume, &pair.subvolume).or_else(|| known.cloned()),
        }
    };
    let mut outcome = CoverageOutcome {
        kind: CoverageKind::NoBaseline,
        pairs: intent.len(),
        compared_with: None,
        drops: Vec::new(),
        restored: Vec::new(),
        renamed_targets: Vec::new(),
        gone: 0,
        new_pairs: 0,
        recording,
        accept_db: None,
    };
    let Some(baseline) = baseline else {
        let rows = intent
            .iter()
            .map(|(p, l)| covered(p, l, today, None))
            .collect();
        let state = CoverageState {
            first_recorded: today.to_string(),
            last_recorded: today.to_string(),
            checks: 1,
        };
        return (outcome, rows, state);
    };
    outcome.kind = CoverageKind::Compared;
    outcome.compared_with = Some(baseline.state.clone());
    let (rows, renamed) = carry_over_renames(&baseline.rows, now);
    outcome.renamed_targets = renamed;
    let followed = follow_renames(rows, listings);
    let recorded = outcome.recording == Recording::Recorded;

    let mut next: Vec<CoverageRow> = Vec::new();
    let mut seen: BTreeSet<Pair> = BTreeSet::new();
    let mut drops: BTreeMap<(String, String, String, String, u32), Vec<String>> = BTreeMap::new();
    let mut restored: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
    // Pairs a row holds already: two rows never end on one.
    let mut claimed: BTreeSet<Pair> = BTreeSet::new();
    for Followed { row: r, was, .. } in &followed {
        let pair = Pair {
            volume: r.volume.clone(),
            subvolume: r.subvolume.clone(),
            target: r.target.clone(),
        };
        if !claimed.insert(pair.clone()) {
            // Another subvolume's row holds this name now; this one's
            // subvolume is no longer anywhere it can be found.
            outcome.gone += 1;
            continue;
        }
        if let Some(source_label) = intent.get(&pair) {
            if let Some(since) = &r.dropped_since {
                restored
                    .entry((r.volume.clone(), r.subvolume.clone(), since.clone()))
                    .or_default()
                    .push(r.target.clone());
            }
            next.push(covered(
                &pair,
                source_label,
                &r.first_covered,
                r.subvol_uuid.as_ref(),
            ));
            seen.insert(pair);
            continue;
        }
        let Some(why) = why_dropped(r, now, plan, listings) else {
            outcome.gone += 1;
            continue;
        };
        let why = match was {
            Some(old) => format!("renamed from '{old}'; {why}"),
            None => why,
        };
        let since = r.dropped_since.clone().unwrap_or_else(|| today.to_string());
        let checks = r.dropped_checks + 1;
        let runs = if recorded { checks } else { r.dropped_checks };
        drops
            .entry((
                r.volume.clone(),
                r.subvolume.clone(),
                why.clone(),
                since.clone(),
                runs,
            ))
            .or_default()
            .push(r.target.clone());
        // The identity follows the target while it is configured, as a
        // covered row's does: one recorded under an older mount_uuid or
        // first serial would leave its label with two identities, and a
        // later rename on the same drive would not be recognised. A target
        // no longer configured keeps the identity it is recognised by.
        let target_identity = match now.targets.iter().find(|t| t.label == r.target) {
            Some(t) => t.coverage_identity(),
            None => r.target_identity.clone(),
        };
        next.push(CoverageRow {
            target_identity,
            dropped_since: Some(since),
            dropped_checks: checks,
            drop_rule: Some(why),
            subvol_uuid: uuid_now(&r.volume, &r.subvolume).or_else(|| r.subvol_uuid.clone()),
            ..r.clone()
        });
    }
    for (pair, source_label) in &intent {
        if !seen.contains(pair) {
            next.push(covered(pair, source_label, today, None));
            outcome.new_pairs += 1;
        }
    }
    outcome.drops = drops
        .into_iter()
        .map(|((volume, subvolume, why, since, runs), mut targets)| {
            targets.sort();
            DropBlock {
                volume,
                subvolume,
                targets,
                why,
                since,
                runs,
            }
        })
        .collect();
    outcome.restored = restored
        .into_iter()
        .map(|((volume, subvolume, since), mut targets)| {
            targets.sort();
            Restored {
                volume,
                subvolume,
                targets,
                since,
            }
        })
        .collect();
    let state = CoverageState {
        first_recorded: baseline.state.first_recorded.clone(),
        last_recorded: today.to_string(),
        checks: baseline.state.checks + 1,
    };
    (outcome, next, state)
}

/// The coverage check a sync runs, against the index database at `db`.
///
/// `now` is the configuration that holds after the sync (re-read from disk on
/// a real sync; what a real one would leave, on a dry run). Only `record`
/// writes: it opens (creating and migrating) the database and compares and
/// records in one transaction. Otherwise the database is read read-only, and
/// a missing one is never created. Nothing here fails the sync: every error
/// becomes a WARN in the outcome.
pub fn check(
    db: &Path,
    record: bool,
    dry_run: bool,
    now: Result<Config, String>,
    plan: &SyncPlan,
    listings: &[VolumeListing],
    today: &str,
) -> CoverageOutcome {
    let mut outcome = check_against(db, record, dry_run, now, plan, listings, today);
    outcome.accept_db = accept_db_for(db);
    outcome
}

/// The database the accept line must name: `db`, unless it is the one
/// `accept`'s `--db` defaults to.
fn accept_db_for(db: &Path) -> Option<String> {
    (db != Path::new(crate::db::DEFAULT_DB_PATH)).then(|| db.display().to_string())
}

/// [`check`] without the accept line's database.
fn check_against(
    db: &Path,
    record: bool,
    dry_run: bool,
    now: Result<Config, String>,
    plan: &SyncPlan,
    listings: &[VolumeListing],
    today: &str,
) -> CoverageOutcome {
    let now = match now {
        Ok(now) => now,
        Err(e) => return CoverageOutcome::not_checked(e),
    };
    // SQLite opens an empty name as a private temporary database: a record
    // written there is gone at close, and a read finds no record, so every
    // run would read as the first. Never a check, always a warning.
    if db.as_os_str().is_empty() {
        return CoverageOutcome::not_checked("no index database path given".to_string());
    }
    let compare = |baseline: Option<Baseline>, recording: Recording| {
        evaluate(baseline.as_ref(), &now, plan, listings, today, recording)
    };
    let recording = if record {
        let database = match Database::open(db) {
            Ok(database) => database,
            Err(e) => {
                return CoverageOutcome::not_checked(format!(
                    "index database {}: {e}",
                    db.display()
                ));
            }
        };
        match database.record_coverage(|b| compare(b, Recording::Recorded)) {
            Ok(outcome) => return outcome,
            Err(e) => Recording::Failed(e),
        }
    } else if dry_run {
        Recording::DryRun
    } else {
        Recording::NotAsked
    };
    match crate::db::read_coverage_read_only(db) {
        Ok(baseline) => compare(baseline, recording).0,
        Err(e) => CoverageOutcome::not_checked(format!("index database {e}")),
    }
}

/// `btrdasd subvol coverage accept`: `(exit code, stdout, stderr)`. 0 when
/// something was accepted, 1 when nothing currently reported as dropped
/// matched (nothing changed, so a typo never silences a later drop), 2 when
/// the database cannot be read or holds no coverage record yet. A database
/// with no record is never opened for writing, so it is not migrated either.
pub fn accept_command(
    db: &Path,
    volume: Option<&str>,
    subvolume: Option<&str>,
    target: Option<&str>,
    accepted_at: &str,
) -> (i32, String, String) {
    let refuse = |why: String| (2, String::new(), format!("Error: {why}\n"));
    match crate::db::read_coverage_read_only(db) {
        Err(e) => return refuse(format!("index database {e}")),
        Ok(None) => {
            return refuse(format!(
                "{} holds no subvolume coverage record yet: nothing to accept",
                db.display()
            ));
        }
        Ok(Some(_)) => {}
    }
    let accepted = Database::open(db)
        .map_err(|e| format!("index database {}: {e}", db.display()))
        .and_then(|database| database.accept_coverage(volume, subvolume, target, accepted_at));
    match accepted {
        Err(e) => refuse(e),
        Ok(list) if list.is_empty() => (
            1,
            "not dropped: nothing accepted (no pair the coverage record holds as no longer backed \
             up matches; a drop shown only by a dry run or a compare-only sync is recorded by \
             the next backup run)\n"
                .to_string(),
            String::new(),
        ),
        Ok(list) => {
            let mut out = format!(
                "Accepted {}; they are no longer tracked:\n",
                if list.len() == 1 {
                    "1 pair".to_string()
                } else {
                    format!("{} pairs", list.len())
                }
            );
            for a in &list {
                out.push_str(&format!(
                    "  {}  {}  -> {}  (no longer backed up since {}: {})\n",
                    a.volume,
                    display_name(&a.subvolume),
                    a.target,
                    a.dropped_since,
                    a.drop_rule
                ));
            }
            (0, out, String::new())
        }
    }
}

/// `btrdasd subvol coverage list`: `(exit code, stdout, stderr)`. The pairs
/// reported as no longer backed up and the accept log, as text or JSON; 0
/// also when there is no record yet, 2 when the database cannot be read.
/// Read-only: the database is never created or migrated.
pub fn list_command(db: &Path, json: bool) -> (i32, String, String) {
    let read = crate::db::read_coverage_read_only(db)
        .and_then(|b| crate::db::read_accepted_read_only(db).map(|a| (b, a)));
    let (baseline, accepted) = match read {
        Ok(read) => read,
        Err(e) => return (2, String::new(), format!("Error: index database {e}\n")),
    };
    let dropped: Vec<&CoverageRow> = baseline
        .iter()
        .flat_map(|b| b.rows.iter())
        .filter(|r| r.dropped_since.is_some())
        .collect();
    if json {
        let value = serde_json::json!({
            "state": baseline.as_ref().map(|b| &b.state),
            "dropped": dropped,
            "accepted": accepted,
        });
        return (0, format!("{value}\n"), String::new());
    }
    let mut out = String::from("SUBVOLUME COVERAGE\n");
    match &baseline {
        None => out.push_str("  No record yet: the next backup run records the first.\n"),
        Some(b) => out.push_str(&format!(
            "  Recorded {} ({} since {})\n",
            b.state.last_recorded,
            if b.state.checks == 1 {
                "1 recorded check".to_string()
            } else {
                format!("{} recorded checks", b.state.checks)
            },
            b.state.first_recorded
        )),
    }
    if dropped.is_empty() {
        out.push_str("  No longer backed up: none\n");
    } else {
        out.push_str("  No longer backed up:\n");
        for r in &dropped {
            out.push_str(&format!(
                "    {}  {}  -> {}  since {}: {}\n",
                r.volume,
                display_name(&r.subvolume),
                r.target,
                r.dropped_since.as_deref().unwrap_or_default(),
                r.drop_rule.as_deref().unwrap_or_default()
            ));
        }
    }
    if accepted.is_empty() {
        out.push_str("  Accepted: none\n");
    } else {
        out.push_str("  Accepted:\n");
        for a in &accepted {
            out.push_str(&format!(
                "    {}  {}  {}  -> {}  (no longer backed up since {}: {})\n",
                a.accepted_at,
                a.volume,
                display_name(&a.subvolume),
                a.target,
                a.dropped_since,
                a.drop_rule
            ));
        }
    }
    (0, out, String::new())
}

/// `epoch` seconds as an RFC 3339 UTC timestamp, `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339_utc(epoch: i64) -> String {
    let secs = epoch.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        crate::caldate::date_of(epoch.div_euclid(86_400)),
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

/// Now, as [`rfc3339_utc`] writes it: when a pair was accepted.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    rfc3339_utc(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adopt::{apply_plan, plan_sync};
    use crate::config::{Retention, Source, SubvolConfig, Target, TargetRole};

    const DAY1: &str = "2026-10-02";
    const DAY2: &str = "2026-10-03";
    const DAY3: &str = "2026-10-04";
    const DAY4: &str = "2026-10-05";

    fn target(label: &str, role: TargetRole, serial: &str) -> Target {
        Target {
            label: label.into(),
            serial: serial.into(),
            serials: vec![serial.into()],
            mount_uuid: None,
            mount: format!("/mnt/{label}"),
            role,
            retention: Retention {
                daily: 7,
                ..Default::default()
            },
            display_name: String::new(),
            esp_partuuid: None,
        }
    }

    fn source(label: &str, volume: &str, targets: &[&str], subvols: &[&str]) -> Source {
        Source {
            label: label.into(),
            volume: volume.into(),
            subvolumes: subvols
                .iter()
                .map(|n| SubvolConfig {
                    name: n.to_string(),
                    ..Default::default()
                })
                .collect(),
            device: format!("UUID=uuid-of-{volume}"),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![label.into()],
            target_labels: targets.iter().map(|t| t.to_string()).collect(),
        }
    }

    /// `/ssd` (to every target): @srv, @data, @opt. `/hdd` (to `big`):
    /// media. Targets `big` (primary) and `small` (mirror): 7 pairs.
    fn cfg() -> Config {
        Config {
            targets: vec![
                target("big", TargetRole::Primary, "SERIAL01"),
                target("small", TargetRole::Mirror, "SERIAL02"),
            ],
            sources: vec![
                source("ssd", "/ssd", &[], &["@srv", "@data", "@opt"]),
                source("media", "/hdd", &["big"], &["media"]),
            ],
            ..Config::default()
        }
    }

    fn listing(volume: &str, names: &[&str]) -> VolumeListing {
        VolumeListing {
            volume: volume.into(),
            subvolumes: Ok(names.iter().map(|s| s.to_string()).collect()),
            uuids: BTreeMap::new(),
        }
    }

    fn on_disk() -> Vec<VolumeListing> {
        vec![
            listing("/ssd", &["@srv", "@data", "@opt"]),
            listing("/hdd", &["media"]),
        ]
    }

    /// What a real sync then check does with `cfg` and `listings` on `today`:
    /// the plan, the config the plan leaves, the comparison against it.
    fn step(
        baseline: Option<&Baseline>,
        cfg: &Config,
        listings: &[VolumeListing],
        today: &str,
        recording: Recording,
    ) -> (CoverageOutcome, Baseline) {
        let plan = plan_sync(cfg, listings);
        let now = apply_plan(cfg, &plan, today).expect("the plan applies");
        let (outcome, rows, state) = evaluate(baseline, &now, &plan, listings, today, recording);
        (outcome, Baseline { state, rows })
    }

    /// A recorded check of `cfg` with nothing on record: the first baseline.
    fn first(cfg: &Config) -> Baseline {
        let (outcome, b) = step(None, cfg, &on_disk(), DAY1, Recording::Recorded);
        assert_eq!(outcome.kind, CoverageKind::NoBaseline);
        b
    }

    fn without(mut cfg: Config, name: &str) -> Config {
        for s in &mut cfg.sources {
            s.subvolumes.retain(|e| e.name != name);
        }
        cfg
    }

    fn whys(o: &CoverageOutcome) -> Vec<String> {
        o.drops.iter().map(|b| b.why.clone()).collect()
    }

    // --- pairing by the subvolume's own UUID (bd DAS-Backup-Manager-sp78.5) ---

    /// A fixture subvolume UUID, by number.
    fn uuid(n: u32) -> String {
        format!("5eed{n:04}-0000-4000-8000-{n:012}")
    }

    /// A listing whose subvolumes carry the UUIDs `(path, n)` names.
    fn listing_u(volume: &str, subs: &[(&str, u32)]) -> VolumeListing {
        VolumeListing {
            volume: volume.into(),
            subvolumes: Ok(subs.iter().map(|(p, _)| p.to_string()).collect()),
            uuids: subs
                .iter()
                .map(|(p, n)| (p.to_string(), uuid(*n)))
                .collect(),
        }
    }

    /// `on_disk()` with a UUID on every subvolume: @srv 1, @data 2, @opt 3,
    /// media 4.
    fn on_disk_u() -> Vec<VolumeListing> {
        vec![
            listing_u("/ssd", &[("@srv", 1), ("@data", 2), ("@opt", 3)]),
            listing_u("/hdd", &[("media", 4)]),
        ]
    }

    fn first_u() -> Baseline {
        step(None, &cfg(), &on_disk_u(), DAY1, Recording::Recorded).1
    }

    fn uuid_of_row(b: &Baseline, subvolume: &str, target: &str) -> Option<String> {
        b.rows
            .iter()
            .find(|r| r.subvolume == subvolume && r.target == target)
            .unwrap_or_else(|| panic!("no row {subvolume} -> {target}"))
            .subvol_uuid
            .clone()
    }

    #[test]
    fn a_recorded_check_keeps_each_subvolumes_uuid_and_none_where_the_listing_had_none() {
        let b1 = first_u();
        assert_eq!(uuid_of_row(&b1, "@data", "big"), Some(uuid(2)));
        assert_eq!(uuid_of_row(&b1, "@data", "small"), Some(uuid(2)));
        assert_eq!(uuid_of_row(&b1, "media", "big"), Some(uuid(4)));
        let bare = first(&cfg());
        assert!(bare.rows.iter().all(|r| r.subvol_uuid.is_none()));
    }

    /// The case sp78.5 was filed for: `mv @data @renamed`, the new name
    /// excluded by a pattern. By path the old pair is simply gone.
    #[test]
    fn a_rename_into_an_excluded_name_warns_and_names_the_old_name() {
        let b1 = first_u();
        let mut now = cfg();
        now.subvolumes.exclude = vec!["@renamed".into()];
        let disk = vec![
            listing_u("/ssd", &[("@srv", 1), ("@renamed", 2), ("@opt", 3)]),
            listing_u("/hdd", &[("media", 4)]),
        ];
        let (o, b2) = step(Some(&b1), &now, &disk, DAY2, Recording::Recorded);
        assert_eq!(o.gone, 0, "{o:?}");
        assert_eq!(
            o.drops,
            vec![DropBlock {
                volume: "/ssd".into(),
                subvolume: "@renamed".into(),
                targets: vec!["big".into(), "small".into()],
                why: "renamed from '@data'; excluded by pattern '@renamed' \
                      ([subvolumes].exclude); no config entry names it"
                    .into(),
                since: DAY2.into(),
                runs: 1,
            }]
        );
        assert!(o.has_warnings());
        // The record follows the subvolume to its new name, UUID kept.
        assert_eq!(uuid_of_row(&b2, "@renamed", "big"), Some(uuid(2)));
        assert!(b2.rows.iter().all(|r| r.subvolume != "@data"));
    }

    /// A rename sync adopts is followed, not read as one gone and one new:
    /// what the old name had on a target the adopted entry does not reach is
    /// the finding.
    #[test]
    fn a_rename_that_sync_adopts_is_followed_and_only_the_narrowing_warns() {
        let b1 = first_u();
        let disk = vec![
            listing_u("/ssd", &[("@srv", 1), ("@renamed", 2), ("@opt", 3)]),
            listing_u("/hdd", &[("media", 4)]),
        ];
        let (o, b2) = step(Some(&b1), &cfg(), &disk, DAY2, Recording::Recorded);
        assert_eq!((o.gone, o.new_pairs), (0, 0), "{o:?}");
        assert_eq!(o.drops.len(), 1, "{o:?}");
        let block = &o.drops[0];
        assert_eq!(
            (block.subvolume.as_str(), block.targets.as_slice()),
            ("@renamed", ["small".to_string()].as_slice())
        );
        assert!(
            block.why.starts_with(
                "renamed from '@data'; its entry left source 'ssd' and was re-adopted"
            ),
            "{}",
            block.why
        );
        let kept = b2
            .rows
            .iter()
            .find(|r| r.subvolume == "@renamed" && r.target == "big")
            .unwrap();
        assert_eq!(
            (kept.first_covered.as_str(), kept.dropped_since.as_deref()),
            (DAY1, None)
        );
    }

    #[test]
    fn two_subvolumes_swapping_names_stay_covered() {
        let b1 = first_u();
        let disk = vec![
            listing_u("/ssd", &[("@srv", 1), ("@data", 3), ("@opt", 2)]),
            listing_u("/hdd", &[("media", 4)]),
        ];
        let (o, b2) = step(Some(&b1), &cfg(), &disk, DAY2, Recording::Recorded);
        assert_eq!((o.gone, o.new_pairs, o.dropped_pairs()), (0, 0, 0), "{o:?}");
        assert_eq!(uuid_of_row(&b2, "@data", "big"), Some(uuid(3)));
        assert_eq!(uuid_of_row(&b2, "@opt", "big"), Some(uuid(2)));
        assert_eq!(b2.rows.len(), 7);
    }

    /// A row no earlier binary gave a UUID is paired by path, as before — a
    /// missing UUID never reads as covered — and the first recorded check
    /// that can read the UUID stamps it.
    #[test]
    fn a_row_with_no_uuid_is_paired_by_path_and_gets_one_at_the_next_record() {
        let mut b1 = first_u();
        for r in &mut b1.rows {
            r.subvol_uuid = None;
        }
        // The same rename, into an excluded name: no UUID, so nothing links
        // @data to @renamed, and the old pair is gone (the old behaviour).
        let mut now = cfg();
        now.subvolumes.exclude = vec!["@renamed".into()];
        let renamed = vec![
            listing_u("/ssd", &[("@srv", 1), ("@renamed", 2), ("@opt", 3)]),
            listing_u("/hdd", &[("media", 4)]),
        ];
        let (o, _) = step(Some(&b1), &now, &renamed, DAY2, Recording::Recorded);
        assert_eq!((o.gone, o.dropped_pairs()), (2, 0), "{o:?}");
        // Unchanged disk: nothing drops, and every row now holds its UUID.
        let (o, b2) = step(Some(&b1), &cfg(), &on_disk_u(), DAY2, Recording::Recorded);
        assert_eq!(o.dropped_pairs(), 0, "{o:?}");
        assert_eq!(uuid_of_row(&b2, "@srv", "small"), Some(uuid(1)));
        assert_eq!(uuid_of_row(&b2, "media", "big"), Some(uuid(4)));
    }

    /// A volume read without UUIDs (or not read at all) cannot move a row,
    /// and cannot erase the UUID it holds.
    #[test]
    fn a_listing_without_uuids_keeps_the_uuid_a_row_holds_and_moves_nothing() {
        let b1 = first_u();
        let (o, b2) = step(Some(&b1), &cfg(), &on_disk(), DAY2, Recording::Recorded);
        assert_eq!((o.gone, o.new_pairs, o.dropped_pairs()), (0, 0, 0), "{o:?}");
        assert_eq!(uuid_of_row(&b2, "@data", "big"), Some(uuid(2)));
    }

    /// The row's UUID is on disk at another path, and an old row still holds
    /// that path: the two never collapse onto one pair.
    #[test]
    fn a_name_taken_over_by_another_subvolume_never_yields_two_rows_for_one_pair() {
        let b1 = first_u();
        // @opt (3) deleted; @data (2) renamed to @opt. By UUID @data's row
        // moves to @opt, and the old @opt row (UUID 3, gone) is not a second
        // claim on the pair.
        let disk = vec![
            listing_u("/ssd", &[("@srv", 1), ("@opt", 2)]),
            listing_u("/hdd", &[("media", 4)]),
        ];
        let (o, b2) = step(Some(&b1), &cfg(), &disk, DAY2, Recording::Recorded);
        let opt_rows = b2.rows.iter().filter(|r| r.subvolume == "@opt").count();
        assert_eq!(o.gone, 2, "{o:?}");
        assert_eq!(opt_rows, 2, "one per target: {:?}", b2.rows);
        assert_eq!(uuid_of_row(&b2, "@opt", "big"), Some(uuid(2)));
        assert_eq!(o.dropped_pairs(), 0, "{o:?}");
    }

    // --- the comparison -------------------------------------------------------

    #[test]
    fn unchanged_config_twice_reports_nothing() {
        let b1 = first(&cfg());
        assert_eq!(b1.rows.len(), 7);
        let (o, b2) = step(Some(&b1), &cfg(), &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(o.kind, CoverageKind::Compared);
        assert_eq!(o.row(), "OK  (7 subvolume/target pairs, none dropped)");
        assert!(!o.has_warnings());
        assert_eq!((o.gone, o.new_pairs), (0, 0));
        assert_eq!(b2.state.checks, 2);
        assert_eq!(b2.state.first_recorded, DAY1);
        assert_eq!(b2.state.last_recorded, DAY2);
        assert!(b2.rows.iter().all(|r| r.last_covered == DAY2
            && r.first_covered == DAY1
            && r.dropped_since.is_none()));
        let (o, _) = step(Some(&b2), &cfg(), &on_disk(), DAY3, Recording::Recorded);
        assert!(!o.has_warnings(), "{}", o.section());
        assert_eq!(
            o.compared_with,
            Some(CoverageState {
                first_recorded: DAY1.into(),
                last_recorded: DAY2.into(),
                checks: 2
            })
        );
        assert!(
            o.section().contains(
                "  Compared with the record of 2026-10-03 (2 recorded checks since 2026-10-02)\n"
            ),
            "{}",
            o.section()
        );
    }

    /// The case the issue was filed for (corrected 2026-11-01): the entry is
    /// removed, and a pattern then keeps sync from adopting it again.
    #[test]
    fn removed_entry_swallowed_by_pattern_warns() {
        let b1 = first(&cfg());
        let mut now = without(cfg(), "@data");
        now.subvolumes.exclude = vec!["@d*".into()];
        let (o, b2) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(
            o.drops,
            vec![DropBlock {
                volume: "/ssd".into(),
                subvolume: "@data".into(),
                targets: vec!["big".into(), "small".into()],
                why: "excluded by pattern '@d*' ([subvolumes].exclude); no config entry names it"
                    .into(),
                since: DAY2.into(),
                runs: 1,
            }]
        );
        assert_eq!(
            o.row(),
            "WARN  (2 subvolume/target pairs no longer backed up)"
        );
        assert_eq!(o.status(), "WARN");
        let dropped: Vec<_> = b2
            .rows
            .iter()
            .filter(|r| r.dropped_since.is_some())
            .collect();
        assert_eq!(dropped.len(), 2);
        assert!(
            dropped.iter().all(|r| r.dropped_checks == 1
                && r.last_covered == DAY1
                && r.source_label == "ssd")
        );
    }

    #[test]
    fn widened_pattern_entry_kept_warns_nothing() {
        let b1 = first(&cfg());
        let mut now = cfg();
        now.subvolumes.exclude = vec!["@d*".into(), "@*".into()];
        now.doctor.exclude = vec!["*".into()];
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(o.row(), "OK  (7 subvolume/target pairs, none dropped)");
    }

    #[test]
    fn every_list_that_holds_the_pattern_is_named() {
        let mut was = cfg();
        was.sources[1].subvolumes.push(SubvolConfig {
            name: "@tmp".into(),
            ..Default::default()
        });
        let disk = vec![
            listing("/ssd", &["@srv", "@data", "@opt"]),
            listing("/hdd", &["media", "@tmp"]),
        ];
        let (_, b1) = step(None, &was, &disk, DAY1, Recording::Recorded);
        let mut now = without(was, "@tmp");
        now.doctor.exclude = vec!["@tmp".into()];
        let (o, _) = step(Some(&b1), &now, &disk, DAY2, Recording::Recorded);
        assert_eq!(
            whys(&o),
            ["excluded by pattern '@tmp' (built-in, [doctor].exclude); no config entry names it"]
        );
    }

    #[test]
    fn a_subvolume_deleted_from_a_read_volume_is_gone_not_dropped() {
        let b1 = first(&cfg());
        let disk = vec![
            listing("/ssd", &["@srv", "@opt"]),
            listing("/hdd", &["media"]),
        ];
        let (o, b2) = step(Some(&b1), &cfg(), &disk, DAY2, Recording::Recorded);
        assert_eq!(o.gone, 2, "{o:?}");
        assert!(o.drops.is_empty() && !o.has_warnings(), "{}", o.section());
        assert_eq!(b2.rows.len(), 5, "gone rows leave the baseline");
        assert!(
            o.section().contains(
                "  Gone from disk (retired by sync, not a warning): 2 subvolume/target pairs\n"
            ),
            "{}",
            o.section()
        );
    }

    #[test]
    fn a_removed_entry_on_a_volume_not_read_is_unread_not_gone() {
        let b1 = first(&cfg());
        let disk = vec![
            VolumeListing {
                volume: "/ssd".into(),
                subvolumes: Err("not mounted".into()),
                uuids: BTreeMap::new(),
            },
            listing("/hdd", &["media"]),
        ];
        let (o, _) = step(
            Some(&b1),
            &without(cfg(), "@data"),
            &disk,
            DAY2,
            Recording::Recorded,
        );
        assert_eq!(o.gone, 0);
        assert_eq!(
            whys(&o),
            [
                "no config entry names it; volume /ssd was not read this run (see VOLUMES NOT \
              READ), so whether it is excluded or still exists was not checked"
            ]
        );
        // A volume with no listing at all is unread too.
        let disk = vec![listing("/hdd", &["media"])];
        let (o, _) = step(
            Some(&b1),
            &without(cfg(), "@data"),
            &disk,
            DAY2,
            Recording::Recorded,
        );
        assert_eq!(o.dropped_pairs(), 2, "{o:?}");
    }

    #[test]
    fn a_target_side_change_on_an_unread_volume_still_warns_and_says_so() {
        let b1 = first(&cfg());
        let mut now = cfg();
        now.sources[0].target_labels = vec!["big".into()];
        let disk = vec![
            VolumeListing {
                volume: "/ssd".into(),
                subvolumes: Err("not mounted".into()),
                uuids: BTreeMap::new(),
            },
            listing("/hdd", &["media"]),
        ];
        let (o, _) = step(Some(&b1), &now, &disk, DAY2, Recording::Recorded);
        assert_eq!(
            whys(&o),
            ["source 'ssd' no longer sends to 'small' (it sends to primary target only) (volume \
              /ssd was not read this run)"; 3]
        );
    }

    #[test]
    fn a_top_level_is_never_gone() {
        let mut was = cfg();
        was.sources[1].subvolumes.push(SubvolConfig {
            name: TOP_LEVEL.into(),
            ..Default::default()
        });
        let (_, b1) = step(None, &was, &on_disk(), DAY1, Recording::Recorded);
        // The listing names no "." (the fixtures' never do); its entry is gone.
        let (o, _) = step(
            Some(&b1),
            &without(was, TOP_LEVEL),
            &on_disk(),
            DAY2,
            Recording::Recorded,
        );
        assert_eq!(o.gone, 0, "{o:?}");
        assert_eq!(o.dropped_pairs(), 1);
        assert!(
            o.section().contains("    /hdd  . (top level)  -> big\n"),
            "{}",
            o.section()
        );
    }

    #[test]
    fn the_only_source_on_a_volume_removed_names_the_volume_and_its_last_source() {
        let b1 = first(&cfg());
        let mut now = cfg();
        now.sources.truncate(1);
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(
            whys(&o),
            [
                "no [[source]] names volume /hdd any more (last in source 'media'); whether it \
              still exists was not checked"
            ]
        );
    }

    #[test]
    fn a_source_removed_from_a_shared_volume_is_re_adopted_for_the_primary_only() {
        let mut was = cfg();
        was.sources[0].subvolumes.retain(|e| e.name != "@data");
        was.sources
            .push(source("ssd-data", "/ssd", &[], &["@data"]));
        let (_, b1) = step(None, &was, &on_disk(), DAY1, Recording::Recorded);
        let mut now = was.clone();
        now.sources.retain(|s| s.label != "ssd-data");
        let (o, b2) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(
            o.drops,
            vec![DropBlock {
                volume: "/ssd".into(),
                subvolume: "@data".into(),
                targets: vec!["small".into()],
                why: "its entry left source 'ssd-data' and was re-adopted into 'ssd-adopted', \
                      which sends to primary target only"
                    .into(),
                since: DAY2.into(),
                runs: 1,
            }]
        );
        let primary = b2
            .rows
            .iter()
            .find(|r| r.subvolume == "@data" && r.target == "big")
            .unwrap();
        assert_eq!(
            primary.dropped_since, None,
            "the primary pair stays covered"
        );
        assert_eq!(primary.source_label, "ssd-adopted", "its source refreshed");
    }

    #[test]
    fn narrowed_target_labels_scope_a_target_out() {
        let b1 = first(&cfg());
        let mut now = cfg();
        now.sources[0].target_labels = vec!["big".into()];
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(o.dropped_pairs(), 3);
        assert!(o.drops.iter().all(|b| b.targets == ["small"]
            && b.why
                == "source 'ssd' no longer sends to 'small' (it sends to primary target only)"));
    }

    #[test]
    fn target_labels_naming_no_target_send_nowhere() {
        let b1 = first(&cfg());
        let mut now = cfg();
        now.sources[0].target_labels = vec!["nope".into(), "gone".into()];
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(o.dropped_pairs(), 6);
        assert!(
            o.drops.iter().all(|b| b.targets == ["big", "small"]
                && b.why
                    == "source 'ssd' sends to no configured target (target_labels = ['nope', \
                        'gone'])"),
            "{o:?}"
        );
    }

    #[test]
    fn a_removed_target_is_named() {
        let b1 = first(&cfg());
        let mut now = cfg();
        now.targets.truncate(1);
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(o.dropped_pairs(), 3);
        assert!(
            o.drops
                .iter()
                .all(|b| b.why == "target 'small' is no longer configured")
        );
        assert!(o.renamed_targets.is_empty());
    }

    #[test]
    fn a_target_renamed_on_the_same_drive_carries_its_pairs_over() {
        let mut was = cfg();
        was.targets[1].mount_uuid = Some("5eed0008-0000-4000-8000-000000000008".into());
        let (_, b1) = step(None, &was, &on_disk(), DAY1, Recording::Recorded);
        let mut now = was.clone();
        now.targets[1].label = "spare".into();
        now.targets[1].serials = vec!["SERIAL09".into()];
        let (o, b2) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert!(!o.has_warnings(), "{}", o.section());
        assert_eq!(
            o.renamed_targets,
            vec![RenamedTarget {
                from: "small".into(),
                to: "spare".into(),
                pairs: 3
            }]
        );
        assert!(
            o.section().contains(
                "  TARGET RENAMED: 'small' is now 'spare' (same mount UUID or serial): 3 \
                 subvolume/target pairs carried over\n"
            ),
            "{}",
            o.section()
        );
        assert_eq!(o.new_pairs, 0);
        assert!(
            b2.rows
                .iter()
                .filter(|r| r.target == "spare")
                .all(|r| r.first_covered == DAY1)
        );
    }

    /// Review round 1: a dropped row kept the identity it was recorded with,
    /// so once the target's identity changed (here a mount_uuid added to the
    /// same drive) the old label carried two identities and a later rename
    /// on that drive was not recognised: every pair to it warned.
    #[test]
    fn rv_rename_after_identity_change_with_an_open_drop() {
        let b1 = first(&cfg());
        let mut dropped = without(cfg(), "@data");
        dropped.subvolumes.exclude = vec!["@data".into()];
        let (_, b2) = step(Some(&b1), &dropped, &on_disk(), DAY2, Recording::Recorded);
        let mut uuid = dropped.clone();
        uuid.targets[1].mount_uuid = Some("5eed0008-0000-4000-8000-000000000008".into());
        let (o, b3) = step(Some(&b2), &uuid, &on_disk(), DAY3, Recording::Recorded);
        assert_eq!(o.dropped_pairs(), 2, "{}", o.section());
        assert!(
            b3.rows
                .iter()
                .filter(|r| r.target == "small")
                .all(|r| r.target_identity.as_deref()
                    == Some("5eed0008-0000-4000-8000-000000000008")),
            "every row to 'small', the dropped one too, carries its identity now: {:?}",
            b3.rows
        );
        let mut renamed = uuid.clone();
        renamed.targets[1].label = "spare".into();
        let (o, _) = step(Some(&b3), &renamed, &on_disk(), DAY4, Recording::Recorded);
        assert_eq!(
            o.renamed_targets,
            vec![RenamedTarget {
                from: "small".into(),
                to: "spare".into(),
                pairs: 3
            }],
            "{}",
            o.section()
        );
        assert_eq!(
            o.dropped_pairs(),
            2,
            "only the real @data drop: {}",
            o.section()
        );
        assert!(
            o.drops.iter().all(|b| b.subvolume == "@data"),
            "{}",
            o.section()
        );
        // A dropped row to a target no longer configured keeps the identity
        // it had: it is what a rename is recognised by.
        let mut gone = uuid.clone();
        gone.targets.truncate(1);
        let (_, b4) = step(Some(&b3), &gone, &on_disk(), DAY4, Recording::Recorded);
        assert!(
            b4.rows
                .iter()
                .filter(|r| r.target == "small")
                .all(|r| r.target_identity.as_deref()
                    == Some("5eed0008-0000-4000-8000-000000000008"))
        );
    }

    #[test]
    fn a_target_renamed_onto_another_drive_is_a_removal() {
        let b1 = first(&cfg());
        let mut now = cfg();
        now.targets[1].label = "spare".into();
        now.targets[1].serial = "SERIAL09".into();
        now.targets[1].serials = vec!["SERIAL09".into()];
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert!(o.renamed_targets.is_empty());
        assert_eq!(o.dropped_pairs(), 3);
        assert_eq!(o.new_pairs, 3);
        assert!(
            o.drops
                .iter()
                .all(|b| b.why == "target 'small' is no longer configured")
        );
    }

    #[test]
    fn two_old_targets_on_one_new_drive_are_neither_renamed() {
        let mut was = cfg();
        was.targets
            .push(target("tiny", TargetRole::Mirror, "SERIAL02"));
        let (_, b1) = step(None, &was, &on_disk(), DAY1, Recording::Recorded);
        let mut now = cfg();
        now.targets[1].label = "spare".into();
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert!(o.renamed_targets.is_empty(), "{:?}", o.renamed_targets);
        assert_eq!(o.dropped_pairs(), 6);
        // One old target alone on that drive is renamed.
        let (_, b1) = step(None, &cfg(), &on_disk(), DAY1, Recording::Recorded);
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(o.renamed_targets.len(), 1);
    }

    /// A target removed while a still-recorded target takes its drive is a
    /// removal, not a rename: re-keying its rows onto a label that already
    /// holds rows for the same subvolumes would collide on the baseline's
    /// key, and the record would fail on every run.
    #[test]
    fn an_old_target_is_never_renamed_onto_a_label_already_recorded() {
        let b1 = first(&cfg());
        let mut now = cfg();
        now.targets.truncate(1);
        now.targets[0].serial = "SERIAL02".into();
        now.targets[0].serials = vec!["SERIAL02".into()];
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert!(o.renamed_targets.is_empty(), "{:?}", o.renamed_targets);
        assert_eq!(o.dropped_pairs(), 3, "{}", o.section());
        assert!(
            o.drops
                .iter()
                .all(|b| b.why == "target 'small' is no longer configured"),
            "{}",
            o.section()
        );
        // And the record of it is written, twice over, not refused.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let (plan, was) = plan_and_now(&cfg());
        check(&db, true, false, Ok(was), &plan, &on_disk(), DAY1);
        let (plan, now) = plan_and_now(&now);
        for day in [DAY2, DAY3] {
            let o = check(&db, true, false, Ok(now.clone()), &plan, &on_disk(), day);
            assert!(matches!(o.recording, Recording::Recorded), "{o:?}");
            assert!(o.renamed_targets.is_empty(), "{o:?}");
            assert_eq!(o.dropped_pairs(), 3, "{o:?}");
        }
    }

    #[test]
    fn an_old_target_whose_rows_disagree_on_identity_is_not_renamed() {
        let b1 = first(&cfg());
        let mut b1 = b1;
        b1.rows
            .iter_mut()
            .find(|r| r.target == "small")
            .unwrap()
            .target_identity = Some("SERIAL07".into());
        let mut now = cfg();
        now.targets[1].label = "spare".into();
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert!(o.renamed_targets.is_empty());
        // And rows with no identity at all carry nothing over either.
        let mut b2 = first(&cfg());
        for r in b2.rows.iter_mut() {
            r.target_identity = None;
        }
        let (o, _) = step(Some(&b2), &now, &on_disk(), DAY2, Recording::Recorded);
        assert!(o.renamed_targets.is_empty());
    }

    #[test]
    fn a_drop_repeats_every_recorded_run_and_counts_them() {
        let b1 = first(&cfg());
        let mut now = without(cfg(), "@data");
        now.subvolumes.exclude = vec!["@data".into()];
        let (_, b2) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        let (_, b3) = step(Some(&b2), &now, &on_disk(), DAY3, Recording::Recorded);
        let (o, b4) = step(Some(&b3), &now, &on_disk(), DAY4, Recording::Recorded);
        assert_eq!(o.drops.len(), 1);
        assert_eq!((o.drops[0].since.as_str(), o.drops[0].runs), (DAY2, 3));
        assert!(
            o.section()
                .contains("      since:  2026-10-03, 3 recorded runs\n"),
            "{}",
            o.section()
        );
        assert!(
            b4.rows
                .iter()
                .filter(|r| r.subvolume == "@data")
                .all(|r| r.dropped_checks == 3)
        );
        // Not recorded: the stored count, and a new drop is in none yet.
        let (o, _) = step(Some(&b3), &now, &on_disk(), DAY4, Recording::DryRun);
        assert_eq!(o.drops[0].runs, 2);
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::NotAsked);
        assert_eq!(o.drops[0].runs, 0);
        assert!(
            o.section()
                .contains("      since:  2026-10-03, not yet in a recorded run\n"),
            "{}",
            o.section()
        );
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert!(
            o.section().contains(", 1 recorded run\n"),
            "{}",
            o.section()
        );
    }

    #[test]
    fn a_restored_pair_is_said_once_then_silent() {
        let b1 = first(&cfg());
        let mut now = without(cfg(), "@data");
        now.subvolumes.exclude = vec!["@data".into()];
        let (_, b2) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        let (o, b3) = step(Some(&b2), &cfg(), &on_disk(), DAY3, Recording::Recorded);
        assert!(!o.has_warnings());
        assert_eq!(
            o.restored,
            vec![Restored {
                volume: "/ssd".into(),
                subvolume: "@data".into(),
                targets: vec!["big".into(), "small".into()],
                since: DAY2.into(),
            }]
        );
        assert!(
            o.section().contains(
                "  RESTORED:  /ssd  @data  -> big, small  (no longer backed up since 2026-10-03)\n"
            ),
            "{}",
            o.section()
        );
        assert!(
            b3.rows
                .iter()
                .all(|r| r.dropped_since.is_none() && r.dropped_checks == 0)
        );
        let (o, _) = step(Some(&b3), &cfg(), &on_disk(), DAY4, Recording::Recorded);
        assert!(o.restored.is_empty());
        assert!(!o.section().contains("RESTORED"));
    }

    #[test]
    fn source_label_and_snapshot_name_changes_say_nothing() {
        let b1 = first(&cfg());
        let mut now = cfg();
        now.sources[0].label = "nvme".into();
        now.sources[0].subvolumes[0].snapshot_name = Some("renamed".into());
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(o.row(), "OK  (7 subvolume/target pairs, none dropped)");
    }

    #[test]
    fn an_after_index_entry_is_scheduled_and_its_drop_warns() {
        let mut was = cfg();
        was.sources[0].subvolumes[1].after_index = true; // @data
        let (_, b1) = step(None, &was, &on_disk(), DAY1, Recording::Recorded);
        assert_eq!(b1.rows.len(), 7);
        let mut now = without(was, "@data");
        now.subvolumes.exclude = vec!["@data".into()];
        let (o, _) = step(Some(&b1), &now, &on_disk(), DAY2, Recording::Recorded);
        assert_eq!(o.dropped_pairs(), 2);
    }

    /// What is scheduled is what the two rendered files send: `btrbk.conf`'s
    /// pairs plus the state conf's (`after_index`).
    #[test]
    fn intended_is_the_main_and_the_state_conf_pairs() {
        let mut c = cfg();
        c.sources[0].subvolumes[1].after_index = true;
        c.sources[1].subvolumes.push(SubvolConfig {
            name: "old".into(),
            retired: Some("2026-09-01".into()),
            ..Default::default()
        });
        let key = |p: &crate::btrbk_conf::DeclaredPair<'_>| Pair {
            volume: p.source.volume.clone(),
            subvolume: p.subvolume.name.clone(),
            target: p.target.label.clone(),
        };
        let mut state_only = c.clone();
        for s in &mut state_only.sources {
            for e in &mut s.subvolumes {
                if e.after_index {
                    e.after_index = false;
                } else {
                    e.retired = Some("2026-09-01".into());
                }
            }
        }
        let mut want: BTreeSet<Pair> = crate::btrbk_conf::declared_pairs(&c)
            .iter()
            .map(key)
            .collect();
        want.extend(
            crate::btrbk_conf::declared_pairs(&state_only)
                .iter()
                .map(key),
        );
        let got: BTreeSet<Pair> = intended(&c).into_keys().collect();
        assert_eq!(got, want);
        assert_eq!(got.len(), 7);
    }

    #[test]
    fn a_top_level_sent_to_a_mirror_is_not_scheduled() {
        let mut c = cfg();
        c.sources[0].subvolumes.push(SubvolConfig {
            name: TOP_LEVEL.into(),
            ..Default::default()
        });
        c.sources[1].subvolumes.push(SubvolConfig {
            name: TOP_LEVEL.into(),
            ..Default::default()
        });
        let got = intended(&c);
        assert!(
            !got.keys()
                .any(|p| p.volume == "/ssd" && p.subvolume == TOP_LEVEL)
        );
        assert_eq!(
            got.get(&Pair {
                volume: "/hdd".into(),
                subvolume: TOP_LEVEL.into(),
                target: "big".into()
            }),
            Some(&"media".to_string())
        );
    }

    // --- the rules no other test reaches -------------------------------------

    fn dropped_row(subvolume: &str, target: &str, source_label: &str) -> CoverageRow {
        CoverageRow {
            volume: "/ssd".into(),
            subvolume: subvolume.into(),
            target: target.into(),
            target_identity: None,
            source_label: source_label.into(),
            first_covered: DAY1.into(),
            last_covered: DAY1.into(),
            dropped_since: None,
            dropped_checks: 0,
            drop_rule: None,
            subvol_uuid: None,
        }
    }

    #[test]
    fn each_remaining_rule_names_itself() {
        let disk = vec![listing(
            "/ssd",
            &["@srv", "@data", "@opt", ".snapshots/1/snapshot"],
        )];
        let why = |row: &CoverageRow, now: &Config, plan: &SyncPlan| {
            why_dropped(row, now, plan, &disk).unwrap()
        };
        let mut now = without(cfg(), "@data");
        let empty = SyncPlan::default();
        // Unplaceable.
        let plan = SyncPlan {
            unplaceable: vec![crate::adopt::Unplaceable {
                volume: "/ssd".into(),
                name: "@data".into(),
                why: "no primary".into(),
            }],
            ..Default::default()
        };
        assert_eq!(
            why(&dropped_row("@data", "big", "ssd"), &now, &plan),
            "could not be placed — see COULD NOT BE PLACED: no primary"
        );
        // A snapshot tree.
        let plan = plan_sync(&now, &disk);
        assert_eq!(
            why(
                &dropped_row(".snapshots/1/snapshot", "big", "ssd"),
                &now,
                &plan
            ),
            "inside a snapshot directory (.snapshots or .btrbk-snapshots); no config entry names it"
        );
        // Adoption planned and not written.
        assert_eq!(
            why(&dropped_row("@data", "big", "ssd"), &now, &plan),
            "its adoption was planned but not written — see SUBVOLUME SYNC"
        );
        // Nothing explains it.
        assert_eq!(
            why(&dropped_row("@data", "big", "ssd"), &now, &empty),
            "no rule found that explains it (report this)"
        );
        // Retired although present.
        now.sources[0].subvolumes.push(SubvolConfig {
            name: "@data".into(),
            retired: Some(DAY1.into()),
            ..Default::default()
        });
        assert_eq!(
            why(&dropped_row("@data", "big", "ssd"), &now, &empty),
            "its entry in source 'ssd' is retired although it is still on disk — see SUBVOLUME SYNC"
        );
        // Live, sent to the target, and still not scheduled: a top level
        // under a mirror-reaching source.
        let mut now = cfg();
        now.sources[0].subvolumes.push(SubvolConfig {
            name: TOP_LEVEL.into(),
            ..Default::default()
        });
        assert_eq!(
            why(&dropped_row(TOP_LEVEL, "small", "ssd"), &now, &empty),
            "the top level cannot be sent while source 'ssd' reaches a mirror — see COULD NOT BE \
             PLACED"
        );
        assert_eq!(
            why(&dropped_row("@data", "small", "ssd"), &now, &empty),
            "no rule found that explains it (report this)"
        );
        // An adoption source that kept its own label is not a re-adoption.
        let mut now = cfg();
        now.sources[0].label = "ssd-adopted".into();
        now.sources[0].target_labels = vec!["big".into()];
        assert_eq!(
            why(&dropped_row("@data", "small", "ssd-adopted"), &now, &empty),
            "source 'ssd-adopted' no longer sends to 'small' (it sends to primary target only)"
        );
    }

    // --- the wording ---------------------------------------------------------

    fn outcome(kind: CoverageKind, recording: Recording) -> CoverageOutcome {
        CoverageOutcome {
            kind,
            pairs: 1,
            compared_with: None,
            drops: Vec::new(),
            restored: Vec::new(),
            renamed_targets: Vec::new(),
            gone: 0,
            new_pairs: 0,
            recording,
            accept_db: None,
        }
    }

    fn one_drop(volume: &str, subvolume: &str, runs: u32) -> DropBlock {
        DropBlock {
            volume: volume.into(),
            subvolume: subvolume.into(),
            targets: vec!["a".into(), "b".into()],
            why: "w".into(),
            since: DAY1.into(),
            runs,
        }
    }

    #[test]
    fn the_row_says_what_was_found_and_what_was_recorded() {
        use CoverageKind::*;
        use Recording::*;
        let row = |k: CoverageKind, r: Recording| outcome(k, r).row();
        assert_eq!(
            row(NoBaseline, Recorded),
            "OK  (no earlier record: 1 subvolume/target pair; recorded — drops are reported \
             from the next run)"
        );
        assert_eq!(
            row(Compared, Recorded),
            "OK  (1 subvolume/target pair, none dropped)"
        );
        assert_eq!(
            row(Compared, DryRun),
            "OK  (1 subvolume/target pair, none dropped — dry run, not recorded)"
        );
        assert_eq!(
            row(NoBaseline, NotAsked),
            "OK  (no earlier record: 1 subvolume/target pair — not recorded (only a backup run \
             records))"
        );
        assert_eq!(
            row(Compared, Failed("disk\nfull".into())),
            "WARN  (1 subvolume/target pair, none dropped; not recorded: disk full)"
        );
        assert_eq!(
            row(NotChecked("why".into()), Recorded),
            "WARN  (not checked: why)"
        );
        let mut many = outcome(Compared, Recorded);
        many.pairs = 2;
        assert_eq!(many.row(), "OK  (2 subvolume/target pairs, none dropped)");
        many.drops = vec![one_drop("/v", "s", 1)];
        assert_eq!(
            many.row(),
            "WARN  (2 subvolume/target pairs no longer backed up)"
        );
        many.drops[0].targets.truncate(1);
        assert_eq!(
            many.row(),
            "WARN  (1 subvolume/target pair no longer backed up)"
        );
        many.drops.push(one_drop("/v", "t", 1));
        assert_eq!(many.dropped_pairs(), 3);
        // A NoBaseline with drops cannot happen; NotChecked has no suffix.
        let multi = CoverageOutcome::not_checked("line one\n  line two\n".into());
        assert_eq!(multi.row(), "WARN  (not checked: line one line two)");
        assert!(multi.has_warnings());
    }

    #[test]
    fn has_warnings_is_true_exactly_for_a_warn() {
        use CoverageKind::*;
        use Recording::*;
        assert!(!outcome(Compared, Recorded).has_warnings());
        assert!(!outcome(NoBaseline, DryRun).has_warnings());
        assert!(!outcome(Compared, NotAsked).has_warnings());
        assert!(outcome(NotChecked("x".into()), NotAsked).has_warnings());
        assert!(outcome(Compared, Failed("x".into())).has_warnings());
        let mut d = outcome(Compared, Recorded);
        d.drops = vec![one_drop("/v", "s", 1)];
        assert!(d.has_warnings());
        assert_eq!(d.status(), "WARN");
        assert_eq!(outcome(Compared, Recorded).status(), "OK");
        assert!(has_warnings_or_missing(None));
        assert!(!has_warnings_or_missing(Some(&outcome(Compared, Recorded))));
        assert!(has_warnings_or_missing(Some(&d)));
    }

    #[test]
    fn the_section_gives_every_dropped_pair_its_reason_date_and_accept_line() {
        let mut o = outcome(CoverageKind::Compared, Recording::Recorded);
        o.compared_with = Some(CoverageState {
            first_recorded: DAY1.into(),
            last_recorded: DAY1.into(),
            checks: 1,
        });
        o.drops = vec![one_drop("/v'x", "it's", 2), one_drop("/w", TOP_LEVEL, 1)];
        o.gone = 1;
        assert_eq!(
            o.section(),
            "SUBVOLUME COVERAGE: WARN  (4 subvolume/target pairs no longer backed up)\n\
             \x20 Compared with the record of 2026-10-02 (1 recorded check since 2026-10-02)\n\
             \x20 NO LONGER BACKED UP:\n\
             \x20   /v'x  it's  -> a, b\n\
             \x20     why:    w\n\
             \x20     since:  2026-10-02, 2 recorded runs\n\
             \x20     accept: btrdasd subvol coverage accept '/v'\\''x' 'it'\\''s'\n\
             \x20   /w  . (top level)  -> a, b\n\
             \x20     why:    w\n\
             \x20     since:  2026-10-02, 1 recorded run\n\
             \x20     accept: btrdasd subvol coverage accept '/w' '.'\n\
             \x20 Not intended? Fix what the \"why\" line names; the next backup run reports it RESTORED.\n\
             \x20 Intended?     Run its accept line (add --target LABEL for one target, or use --all for every pair above).\n\
             \x20 Gone from disk (retired by sync, not a warning): 1 subvolume/target pair\n"
        );
        let quiet = outcome(CoverageKind::Compared, Recording::Recorded);
        assert_eq!(
            quiet.section(),
            "SUBVOLUME COVERAGE: OK  (1 subvolume/target pair, none dropped)\n"
        );
        assert_eq!(section(Some(&quiet)), quiet.section());
    }

    #[test]
    fn no_outcome_is_the_cautious_not_run_warning() {
        assert_eq!(
            row_or_not_run(None),
            "WARN  (not checked: subvolume sync could not run)"
        );
        assert_eq!(
            section(None),
            "SUBVOLUME COVERAGE: WARN  (not checked: subvolume sync could not run)\n"
        );
        let quiet = outcome(CoverageKind::Compared, Recording::Recorded);
        assert_eq!(row_or_not_run(Some(&quiet)), quiet.row());
    }

    /// `backup-run.sh` reads the row by its prefix and writes the not-run
    /// detail itself: both texts must stay what this module prints.
    #[test]
    fn the_script_holds_the_section_prefix_and_the_not_run_detail() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/backup-run.sh");
        let script = std::fs::read_to_string(&path).unwrap();
        assert!(
            script.contains(&format!("\"{NOT_RUN_DETAIL}\"")),
            "{} lacks \"{NOT_RUN_DETAIL}\"",
            path.display()
        );
        assert!(
            script.contains(&format!("prefix=\"{SECTION_PREFIX}\"")),
            "{} does not read the row by '{SECTION_PREFIX}'",
            path.display()
        );
        // The row is `STATUS  (detail)`, two spaces as every other ops row on
        // both paths; the script parses exactly that.
        let row = outcome(CoverageKind::Compared, Recording::Recorded).row();
        assert!(row.starts_with("OK  ("), "{row}");
        assert!(
            script.contains(r#""$row" == "$status  ("*")""#)
                && script.contains(r#"detail="${row#"$status"  (}""#),
            "{} does not parse `STATUS  (detail)`",
            path.display()
        );
    }

    #[test]
    fn the_accept_line_names_the_database_only_when_it_is_not_the_default() {
        assert_eq!(accept_db_for(Path::new(crate::db::DEFAULT_DB_PATH)), None);
        assert_eq!(
            accept_db_for(Path::new("/srv/das/index.db")).as_deref(),
            Some("/srv/das/index.db")
        );
        let mut o = outcome(CoverageKind::Compared, Recording::Recorded);
        o.drops = vec![one_drop("/v", "s", 1)];
        assert!(
            o.section()
                .contains("      accept: btrdasd subvol coverage accept '/v' 's'\n"),
            "{}",
            o.section()
        );
        o.accept_db = Some("/srv/das/index.db".into());
        assert!(
            o.section().contains(
                "      accept: btrdasd subvol coverage accept --db '/srv/das/index.db' '/v' 's'\n"
            ),
            "{}",
            o.section()
        );
    }

    #[test]
    fn timestamps_are_rfc_3339_utc() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(3_661), "1970-01-01T01:01:01Z");
        assert_eq!(rfc3339_utc(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(rfc3339_utc(1_793_500_000), "2026-11-01T02:26:40Z");
        let now = now_rfc3339();
        assert_eq!(now.len(), 20, "{now}");
        assert!(now.as_str() > "2026-01-01T00:00:00Z", "{now}");
    }

    // --- the database edge -----------------------------------------------------

    fn plan_and_now(cfg: &Config) -> (SyncPlan, Config) {
        let plan = plan_sync(cfg, &on_disk());
        let now = apply_plan(cfg, &plan, DAY1).unwrap();
        (plan, now)
    }

    #[test]
    fn check_records_only_when_asked_and_reads_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let (plan, now) = plan_and_now(&cfg());
        // Compare, nothing there: nothing created.
        for dry in [true, false] {
            let o = check(&db, false, dry, Ok(now.clone()), &plan, &on_disk(), DAY1);
            assert_eq!(o.kind, CoverageKind::NoBaseline);
            let want = if dry {
                Recording::DryRun
            } else {
                Recording::NotAsked
            };
            assert_eq!(o.recording, want);
            assert!(!db.exists());
        }
        let o = check(&db, true, false, Ok(now.clone()), &plan, &on_disk(), DAY1);
        assert_eq!(
            (o.kind.clone(), o.recording.clone()),
            (CoverageKind::NoBaseline, Recording::Recorded)
        );
        let o = check(&db, false, false, Ok(now.clone()), &plan, &on_disk(), DAY2);
        assert_eq!(o.kind, CoverageKind::Compared, "a read finds the record");
        assert_eq!(
            crate::db::read_coverage_read_only(&db)
                .unwrap()
                .unwrap()
                .state
                .checks,
            1,
            "a compare writes nothing"
        );
        let o = check(&db, true, false, Ok(now), &plan, &on_disk(), DAY2);
        assert_eq!(o.kind, CoverageKind::Compared);
        assert_eq!(
            crate::db::read_coverage_read_only(&db)
                .unwrap()
                .unwrap()
                .state
                .checks,
            2
        );
    }

    #[test]
    fn check_reports_what_it_could_not_do_as_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let (plan, now) = plan_and_now(&cfg());
        let o = check(
            &dir.path().join("x.db"),
            true,
            false,
            Err("boom".into()),
            &plan,
            &on_disk(),
            DAY1,
        );
        assert_eq!(o.row(), "WARN  (not checked: boom)");
        // An empty path would be SQLite's private temporary database: a
        // record that vanishes, a read that finds none.
        for record in [true, false] {
            let o = check(
                Path::new(""),
                record,
                false,
                Ok(now.clone()),
                &plan,
                &on_disk(),
                DAY1,
            );
            assert_eq!(o.row(), "WARN  (not checked: no index database path given)");
        }
        // A directory cannot be opened as a database, read or write.
        for record in [true, false] {
            let o = check(
                dir.path(),
                record,
                false,
                Ok(now.clone()),
                &plan,
                &on_disk(),
                DAY1,
            );
            assert!(
                matches!(&o.kind, CoverageKind::NotChecked(w) if w.starts_with("index database ")),
                "{o:?}"
            );
        }
    }

    #[test]
    fn a_failed_write_still_shows_the_drops_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let (plan, now) = plan_and_now(&cfg());
        check(&db, true, false, Ok(now), &plan, &on_disk(), DAY1);
        let before = crate::db::read_coverage_read_only(&db).unwrap().unwrap();
        crate::db::Database::open(&db)
            .unwrap()
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_baseline BEFORE INSERT ON coverage_baseline
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let mut gone = without(cfg(), "@data");
        gone.subvolumes.exclude = vec!["@data".into()];
        let plan = plan_sync(&gone, &on_disk());
        let o = check(&db, true, false, Ok(gone), &plan, &on_disk(), DAY2);
        assert_eq!(o.dropped_pairs(), 2, "{o:?}");
        assert!(
            matches!(&o.recording, Recording::Failed(e) if e.contains("injected")),
            "{o:?}"
        );
        assert!(
            o.row()
                .ends_with("no longer backed up; not recorded: injected)"),
            "{}",
            o.row()
        );
        assert_eq!(
            crate::db::read_coverage_read_only(&db).unwrap().unwrap(),
            before
        );
    }

    #[test]
    fn accept_takes_only_dropped_pairs_and_says_what_it_did() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let at = "2026-10-03T08:00:00Z";
        // No file, then no record: nothing to accept, and nothing created.
        let (code, _, err) = accept_command(&db, Some("/ssd"), Some("@data"), None, at);
        assert_eq!(code, 2, "{err}");
        assert!(!db.exists());
        drop(crate::db::Database::open(&db).unwrap());
        let (code, _, err) = accept_command(&db, None, None, None, at);
        assert_eq!(code, 2);
        assert!(
            err.contains("holds no subvolume coverage record yet"),
            "{err}"
        );

        let (plan, now) = plan_and_now(&cfg());
        check(&db, true, false, Ok(now), &plan, &on_disk(), DAY1);
        let mut gone = without(cfg(), "@data");
        gone.subvolumes.exclude = vec!["@data".into()];
        let plan = plan_sync(&gone, &on_disk());
        check(&db, true, false, Ok(gone), &plan, &on_disk(), DAY2);

        // A covered pair is refused, and nothing changes.
        let before = crate::db::read_coverage_read_only(&db).unwrap();
        let (code, out, _) = accept_command(&db, Some("/ssd"), Some("@srv"), None, at);
        assert_eq!(code, 1);
        assert_eq!(
            out,
            "not dropped: nothing accepted (no pair the coverage record holds as no longer backed \
             up matches; a drop shown only by a dry run or a compare-only sync is recorded by \
             the next backup run)\n"
        );
        assert_eq!(crate::db::read_coverage_read_only(&db).unwrap(), before);
        // One target, then the rest.
        let (code, out, _) = accept_command(&db, Some("/ssd"), Some("@data"), Some("small"), at);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "Accepted 1 pair; they are no longer tracked:\n  /ssd  @data  -> small  (no longer \
             backed up since 2026-10-03: excluded by pattern '@data' ([subvolumes].exclude); no \
             config entry names it)\n"
        );
        let (code, out, _) = accept_command(&db, None, None, None, at);
        assert_eq!(code, 0);
        assert!(out.starts_with("Accepted 1 pair;"), "{out}");
        let (code, _, _) = accept_command(&db, None, None, None, at);
        assert_eq!(code, 1, "nothing is left to accept");

        let (code, out, _) = list_command(&db, false);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "SUBVOLUME COVERAGE\n  Recorded 2026-10-03 (2 recorded checks since 2026-10-02)\n  \
             No longer backed up: none\n  Accepted:\n    2026-10-03T08:00:00Z  /ssd  @data  -> \
             small  (no longer backed up since 2026-10-03: excluded by pattern '@data' \
             ([subvolumes].exclude); no config entry names it)\n    2026-10-03T08:00:00Z  /ssd  \
             @data  -> big  (no longer backed up since 2026-10-03: excluded by pattern '@data' \
             ([subvolumes].exclude); no config entry names it)\n"
        );
    }

    /// Review round 1 (critic): a drop only a compare-only sync or a dry run
    /// has seen is not in the record, so accept cannot take it. The report
    /// must not print an accept line for it, and accept's refusal must not
    /// say the pair was never reported.
    #[test]
    fn a_drop_not_yet_recorded_gets_no_accept_line_and_a_refusal_that_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let (plan, now) = plan_and_now(&cfg());
        check(&db, true, false, Ok(now), &plan, &on_disk(), DAY1);
        let mut gone = without(cfg(), "@data");
        gone.subvolumes.exclude = vec!["@data".into()];
        let plan = plan_sync(&gone, &on_disk());
        for dry_run in [false, true] {
            let o = check(
                &db,
                false,
                dry_run,
                Ok(gone.clone()),
                &plan,
                &on_disk(),
                DAY2,
            );
            let text = o.section();
            assert_eq!(o.dropped_pairs(), 2, "{text}");
            assert!(!text.contains("accept: btrdasd"), "{text}");
            assert!(
                text.contains(
                    "      accept: once a backup run has recorded this drop (this report records \
                     nothing)\n"
                ),
                "{text}"
            );
            assert!(!text.contains("Run its accept line"), "{text}");
        }
        let (code, out, _) = accept_command(&db, Some("/ssd"), Some("@data"), None, "T");
        assert_eq!(code, 1);
        assert_eq!(
            out,
            "not dropped: nothing accepted (no pair the coverage record holds as no longer \
             backed up matches; a drop shown only by a dry run or a compare-only sync is \
             recorded by the next backup run)\n"
        );
        // Once recorded, the same report on a compare gives the working line.
        check(&db, true, false, Ok(gone.clone()), &plan, &on_disk(), DAY2);
        let o = check(&db, false, false, Ok(gone), &plan, &on_disk(), DAY3);
        assert!(
            o.section().contains("Run its accept line"),
            "{}",
            o.section()
        );
        assert!(
            o.section()
                .contains("      accept: btrdasd subvol coverage accept"),
            "{}",
            o.section()
        );
    }

    /// Review round 1: the accept line is pasted as printed, and `accept`'s
    /// `--db` defaults to the standard database. A check made against any
    /// other one names it, so the line acts on the database that warned.
    #[test]
    fn the_accept_line_names_a_database_that_is_not_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("it's custom.db");
        let (plan, now) = plan_and_now(&cfg());
        check(&db, true, false, Ok(now), &plan, &on_disk(), DAY1);
        let mut gone = without(cfg(), "@data");
        gone.subvolumes.exclude = vec!["@data".into()];
        let plan = plan_sync(&gone, &on_disk());
        let o = check(&db, true, false, Ok(gone), &plan, &on_disk(), DAY2);
        let quoted = shell_quote(&db.display().to_string());
        let line =
            format!("      accept: btrdasd subvol coverage accept --db {quoted} '/ssd' '@data'\n");
        assert!(o.section().contains(&line), "{}", o.section());
        // That exact line, as argv, accepts.
        let (code, out, err) = accept_command(&db, Some("/ssd"), Some("@data"), None, "T");
        assert_eq!(code, 0, "{out}{err}");
    }

    #[test]
    fn accept_two_at_once_and_list_them_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let (plan, now) = plan_and_now(&cfg());
        check(&db, true, false, Ok(now), &plan, &on_disk(), DAY1);
        let mut gone = without(cfg(), "@data");
        gone.subvolumes.exclude = vec!["@data".into()];
        let plan = plan_sync(&gone, &on_disk());
        check(&db, true, false, Ok(gone), &plan, &on_disk(), DAY2);

        let (code, out, _) = list_command(&db, false);
        assert_eq!(code, 0);
        assert!(
            out.contains(
                "  No longer backed up:\n    /ssd  @data  -> big  since 2026-10-03: \
                          excluded by pattern '@data'"
            ),
            "{out}"
        );
        assert!(out.contains("  Accepted: none\n"), "{out}");
        let (_, json, _) = list_command(&db, true);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["state"]["checks"], 2);
        assert_eq!(v["dropped"].as_array().unwrap().len(), 2);
        assert_eq!(v["accepted"].as_array().unwrap().len(), 0);

        let (code, out, _) = accept_command(&db, Some("/ssd"), Some("@data"), None, "T");
        assert_eq!(code, 0);
        assert!(
            out.starts_with("Accepted 2 pairs; they are no longer tracked:\n"),
            "{out}"
        );
        let (_, json, _) = list_command(&db, true);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["dropped"].as_array().unwrap().len(), 0);
        assert_eq!(v["accepted"][1]["target"], "small");
        assert_eq!(v["accepted"][0]["accepted_at"], "T");
    }

    #[test]
    fn list_without_a_record_says_so_and_an_unreadable_database_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (code, out, _) = list_command(&dir.path().join("none.db"), false);
        assert_eq!(code, 0);
        assert_eq!(
            out,
            "SUBVOLUME COVERAGE\n  No record yet: the next backup run records the first.\n  \
             No longer backed up: none\n  Accepted: none\n"
        );
        let (_, json, _) = list_command(&dir.path().join("none.db"), true);
        assert_eq!(json, "{\"accepted\":[],\"dropped\":[],\"state\":null}\n");
        let (code, out, err) = list_command(dir.path(), false);
        assert_eq!((code, out.as_str()), (2, ""));
        assert!(err.starts_with("Error: index database "), "{err}");
    }
}
