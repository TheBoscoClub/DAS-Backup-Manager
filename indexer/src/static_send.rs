//! Static subvolumes: read-only subvolumes somebody else made (the VM
//! pristines), sent to the primary targets exactly as they are (bd
//! DAS-Backup-Manager-5237.8).
//!
//! A `[[source.static]]` table names a directory of a source's volume and a
//! glob on subvolume names; every read-only subvolume directly in that
//! directory that matches is a candidate. Nothing is ever written on the
//! source: no snapshot, no property change, no delete.
//!
//! On a target the copies land directly in the table's `target_subdir`,
//! received there by `btrfs send | btrfs receive` and checked (received UUID,
//! read-only) before they count. (A received read-only subvolume cannot be
//! renamed out of a staging directory: the kernel answers EROFS, so the copy
//! is received at its final name.) A receive that was cut short leaves a
//! read-write subvolume with no received UUID at that name; so that it can be
//! told from anything somebody else put there, a marker file is written in
//! `<target_subdir>/.incoming/` before each receive and removed once the copy
//! is checked.
//!
//! The only two deletes this module ever makes, each `btrfs subvolume delete`
//! on one exact path built from a verified target mount:
//!
//! 1. at the start of a pass, a read-write subvolume with no received UUID
//!    directly in `target_subdir` whose marker is in `.incoming/`: this
//!    module's own interrupted receive;
//! 2. a superseded copy: a read-only copy with a received UUID whose source
//!    is gone, once a current candidate of the same series has landed and
//!    nothing of that series failed ([`plan_prunes`]).
//!
//! Everything doubtful resolves to keeping: an unreadable listing is an
//! error, never "nothing there".

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::btrbk_conf::{normalise_dir, source_targets};
use crate::config::{Config, StaticSubvolumes, Target, TargetRole};
use crate::fsutil::CommandRunner;

/// The directory under a table's `target_subdir` holding one marker file per
/// receive in progress. Never a subvolume, never a copy.
pub const MARKER_DIR: &str = ".incoming";

/// What a failed static pass is prefixed with in a run's `errors`.
pub const ERROR_PREFIX: &str = "Static subvolumes: ";

// ---------------------------------------------------------------------------
// What `btrfs` says
// ---------------------------------------------------------------------------

/// One subvolume of `btrfs subvolume list -q -u -R [-r]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subvol {
    pub id: u64,
    /// Relative to the filesystem's top level.
    pub path: String,
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub received_uuid: Option<String>,
    pub readonly: bool,
}

impl Subvol {
    /// What this subvolume IS, for the question "is this the same one": its
    /// received UUID when it has one (a copy made by receive carries its
    /// source's identity), else its own UUID. A copy received from a
    /// subvolume carries that subvolume's identity, which is NOT its UUID
    /// when it was itself received (proven on loopback filesystems, bd
    /// DAS-Backup-Manager-5237.8).
    pub fn identity(&self) -> &str {
        self.received_uuid.as_deref().unwrap_or(&self.uuid)
    }

    /// The directory the subvolume is directly in (`""` at the top level).
    pub fn parent_dir(&self) -> &str {
        self.path.rsplit_once('/').map_or("", |(dir, _)| dir)
    }

    pub fn name(&self) -> &str {
        self.path.rsplit_once('/').map_or(&self.path, |(_, n)| n)
    }
}

/// The argv's `btrfs subvolume list` takes: every UUID, and only the
/// read-only subvolumes when `readonly`.
fn list_args(mount: &str, readonly: bool) -> Vec<&str> {
    let mut args = vec!["subvolume", "list", "-q", "-u", "-R"];
    if readonly {
        args.push("-r");
    }
    args.push(mount);
    args
}

/// One parsed listing row: id, path, uuid, parent and received UUID.
type Row = (u64, String, String, Option<String>, Option<String>);

/// Rows of one listing: id, path, uuid, parent and received UUID. An `ID `
/// line that does not read completely is an error — a row guessed at is a
/// subvolume that may be judged the wrong thing — and so is a repeated id.
fn parse_rows(stdout: &str) -> Result<Vec<Row>, String> {
    let uuid_field =
        |value: Option<&str>, what: &str, line: &str| -> Result<Option<String>, String> {
            match value {
                Some("-") => Ok(None),
                Some(u) if crate::adopt::is_uuid(u) => Ok(Some(u.to_string())),
                _ => Err(format!("cannot read the {what} in '{line}'")),
            }
        };
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for line in stdout.lines().filter(|l| l.starts_with("ID ")) {
        let (head, path) = line
            .split_once(" path ")
            .filter(|(_, p)| !p.is_empty())
            .ok_or_else(|| format!("no path in '{line}'"))?;
        let tokens: Vec<&str> = head.split_whitespace().collect();
        let after = |key: &str| {
            tokens
                .iter()
                .position(|t| *t == key)
                .and_then(|i| tokens.get(i + 1).copied())
        };
        let id: u64 = after("ID")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("cannot read the id in '{line}'"))?;
        let uuid = uuid_field(after("uuid"), "uuid", line)?
            .ok_or_else(|| format!("a subvolume with no uuid in '{line}'"))?;
        let parent = uuid_field(after("parent_uuid"), "parent_uuid", line)?;
        let received = uuid_field(after("received_uuid"), "received_uuid", line)?;
        if !seen.insert(id) {
            return Err(format!("subvolume id {id} is listed twice"));
        }
        rows.push((id, path.to_string(), uuid, parent, received));
    }
    Ok(rows)
}

/// The subvolumes under `mount` (the top level of its filesystem), with
/// their read-only flag: two listings, the second the read-only ones. An
/// empty listing is an error on its own — a filesystem that holds these
/// subvolumes never lists none, so "nothing" is the signature of reading the
/// wrong thing (`adopt::normalize_listing`) — and so is a read-only subvolume
/// the full listing does not have.
pub fn read_listing(runner: &dyn CommandRunner, mount: &str) -> Result<Vec<Subvol>, String> {
    let run = |readonly: bool| -> Result<String, String> {
        let args = list_args(mount, readonly);
        match runner.output(Command::new("btrfs").args(&args)) {
            Ok(out) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(out) => Err(format!(
                "'btrfs {}' failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => Err(format!("'btrfs {}' could not be run: {e}", args.join(" "))),
        }
    };
    let all = parse_rows(&run(false)?).map_err(|e| format!("listing {mount}: {e}"))?;
    if all.is_empty() {
        return Err(format!(
            "listing {mount} gave no subvolumes — treating it as unreadable, not as empty"
        ));
    }
    let readonly: BTreeSet<u64> = parse_rows(&run(true)?)
        .map_err(|e| format!("listing the read-only subvolumes of {mount}: {e}"))?
        .into_iter()
        .map(|r| r.0)
        .collect();
    let known: BTreeSet<u64> = all.iter().map(|r| r.0).collect();
    if let Some(stray) = readonly.difference(&known).next() {
        return Err(format!(
            "listing {mount}: read-only subvolume id {stray} is not in the full listing"
        ));
    }
    Ok(all
        .into_iter()
        .map(|(id, path, uuid, parent_uuid, received_uuid)| Subvol {
            id,
            path,
            uuid,
            parent_uuid,
            received_uuid,
            readonly: readonly.contains(&id),
        })
        .collect())
}

/// What `btrfs subvolume show <path>` says of one subvolume: its UUID, its
/// received UUID, whether it is read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub uuid: String,
    pub received_uuid: Option<String>,
    pub readonly: bool,
}

/// Read the labelled lines of `btrfs subvolume show`. A missing label is an
/// error, never a default.
pub fn parse_show(stdout: &str) -> Result<Shown, String> {
    let field = |label: &str| {
        stdout.lines().find_map(|l| {
            let (key, value) = l.split_once(':')?;
            (key.trim() == label).then(|| value.trim().to_string())
        })
    };
    let uuid = field("UUID")
        .filter(|u| crate::adopt::is_uuid(u))
        .ok_or("no UUID in the subvolume's details")?;
    let received = match field("Received UUID").as_deref() {
        Some("-") => None,
        Some(u) if crate::adopt::is_uuid(u) => Some(u.to_string()),
        _ => return Err("no Received UUID in the subvolume's details".into()),
    };
    let flags = field("Flags").ok_or("no Flags in the subvolume's details")?;
    Ok(Shown {
        uuid,
        received_uuid: received,
        readonly: flags.split(',').any(|f| f.trim() == "readonly"),
    })
}

// ---------------------------------------------------------------------------
// Globs and series
// ---------------------------------------------------------------------------

/// Whether `name` matches `pattern`, where only `*` (any run, also empty) and
/// `?` (one character) are special.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|skip| go(rest, &n[skip..])),
            Some(('?', rest)) => n.split_first().is_some_and(|(_, tail)| go(rest, tail)),
            Some((c, rest)) => n
                .split_first()
                .is_some_and(|(first, tail)| first == c && go(rest, tail)),
        }
    }
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    go(&p, &n)
}

/// The series `name` belongs to under `pattern`: the shortest prefix of the
/// name that matches the pattern up to its final `*`, which then matched the
/// rest (`*.pristine-*` and `dev.pristine-2026-10-21-a`: `dev.pristine-`). A
/// copy is replaced only by a candidate of the same series.
///
/// `None` — never a series, so nothing of it is ever pruned — when the name
/// does not match, the pattern does not end in `*`, or what comes before the
/// final `*` holds no literal character (`*` or `?*` name no series at all).
pub fn series_of<'a>(pattern: &str, name: &'a str) -> Option<&'a str> {
    let head = pattern.strip_suffix('*')?.trim_end_matches('*');
    if head.is_empty() || head.chars().all(|c| c == '*' || c == '?') {
        return None;
    }
    if !glob_match(pattern, name) {
        return None;
    }
    name.char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(name.len()))
        .map(|end| &name[..end])
        .find(|prefix| glob_match(head, prefix))
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// What one candidate needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// Not read-only on the source: never sent, never made read-only.
    NotReadOnly { name: String },
    /// A read-only copy of it is on the target already (named `copy`).
    UpToDate { name: String, copy: String },
    /// Something else is in the way on the target; nothing is overwritten.
    Refused { name: String, why: String },
    /// To be sent; `parent` the newest read-only sibling the target holds a
    /// copy of (a full send without one).
    Send {
        name: String,
        identity: String,
        source_path: PathBuf,
        parent: Option<(String, PathBuf)>,
    },
}

/// A table's candidates: the matching subvolumes directly in its directory.
fn candidates<'a>(table: &StaticSubvolumes, source: &'a [Subvol]) -> Vec<&'a Subvol> {
    let dir = normalise_dir(&table.dir);
    let mut found: Vec<&Subvol> = source
        .iter()
        .filter(|s| normalise_dir(s.parent_dir()) == dir && glob_match(&table.pattern, s.name()))
        .collect();
    found.sort_by(|a, b| a.name().cmp(b.name()));
    found
}

/// What each candidate needs, by name. `source` is the whole source volume
/// (the incremental parent may be anywhere on it: btrbk's snapshots of the
/// same VM are outside the directory), `target` the whole target filesystem.
pub fn plan_sends(
    table: &StaticSubvolumes,
    volume: &Path,
    source: &[Subvol],
    target_subdir: &str,
    target: &[Subvol],
) -> Vec<Item> {
    let subdir = normalise_dir(target_subdir);
    let entries: Vec<&Subvol> = target
        .iter()
        .filter(|t| normalise_dir(t.parent_dir()) == subdir)
        .collect();
    candidates(table, source)
        .into_iter()
        .map(|c| {
            let name = c.name().to_string();
            if !c.readonly {
                return Item::NotReadOnly { name };
            }
            let identity = c.identity();
            if let Some(copy) = entries
                .iter()
                .find(|e| e.received_uuid.as_deref() == Some(identity))
            {
                return if copy.readonly {
                    Item::UpToDate {
                        name,
                        copy: copy.name().to_string(),
                    }
                } else {
                    Item::Refused {
                        name,
                        why: format!(
                            "its copy '{}' is not read-only — left as it is",
                            copy.name()
                        ),
                    }
                };
            }
            if let Some(taken) = entries.iter().find(|e| e.name() == name) {
                let what = match (&taken.received_uuid, taken.readonly) {
                    (None, false) => {
                        "an unfinished receive or a subvolume put there by hand (no received UUID, \
                         not read-only)"
                    }
                    (Some(_), _) => "a copy of a different subvolume",
                    (None, true) => "a read-only subvolume that is not a received copy",
                };
                return Item::Refused {
                    name,
                    why: format!(
                        "the name is taken on the target by {what} — nothing is overwritten"
                    ),
                };
            }
            let parent = c.parent_uuid.as_deref().and_then(|parent_uuid| {
                source
                    .iter()
                    .filter(|s| {
                        s.id != c.id
                            && s.readonly
                            && s.parent_uuid.as_deref() == Some(parent_uuid)
                            && target.iter().any(|t| {
                                t.readonly && t.received_uuid.as_deref() == Some(s.identity())
                            })
                    })
                    .max_by_key(|s| s.id)
                    .map(|s| (s.name().to_string(), volume.join(&s.path)))
            });
            Item::Send {
                name,
                identity: identity.to_string(),
                source_path: volume.join(&c.path),
                parent,
            }
        })
        .collect()
}

/// What becomes of a copy on the target whose pristine is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// Deleted: its series has a current copy, checked just now.
    Prune { name: String },
    /// Kept, and said so.
    Kept { name: String, why: String },
}

/// The copies in `entries` (the subvolumes directly in the target
/// directory, listed AFTER this pass's sends) that are superseded and may be
/// deleted, and those that are kept and why. `source` is the whole source
/// volume, `candidates` the table's candidates by name and identity.
///
/// A copy is pruned only when ALL hold, each positively established:
/// it is read-only with a received UUID; no subvolume anywhere on the
/// source has that identity (so it is not a copy of anything present);
/// the table's pattern ends in `*` and names a series for it; at least one
/// candidate of that series has a read-only copy on the target now; and no
/// candidate of that series lacks one (a failed, refused or not-read-only one
/// means the series is not settled). Anything else is kept.
pub fn plan_prunes(
    pattern: &str,
    source: &[Subvol],
    candidates: &[(&str, &str)],
    entries: &[&Subvol],
) -> Vec<Disposition> {
    let landed = |identity: &str| {
        entries
            .iter()
            .any(|e| e.readonly && e.received_uuid.as_deref() == Some(identity))
    };
    let mut out = Vec::new();
    let mut sorted: Vec<&&Subvol> = entries.iter().collect();
    sorted.sort_by(|a, b| a.name().cmp(b.name()));
    for entry in sorted {
        let name = entry.name().to_string();
        let kept = |why: &str| Disposition::Kept {
            name: name.clone(),
            why: why.to_string(),
        };
        let Some(received) = entry.received_uuid.as_deref().filter(|_| entry.readonly) else {
            out.push(kept(
                "not a received read-only copy (read-write, or no received UUID): never deleted here",
            ));
            continue;
        };
        if source.iter().any(|s| s.identity() == received) {
            continue; // its source is there: a current copy
        }
        let Some(series) = series_of(pattern, entry.name()) else {
            out.push(kept(
                "source gone; the pattern names no series for it (it must end in '*' after a literal), so nothing replaces it",
            ));
            continue;
        };
        let same: Vec<&(&str, &str)> = candidates
            .iter()
            .filter(|(candidate, _)| series_of(pattern, candidate) == Some(series))
            .collect();
        if same.is_empty() {
            out.push(kept(
                "source gone, not replaced: no candidate of its series",
            ));
        } else if let Some((unsettled, _)) = same.iter().find(|(_, id)| !landed(id)) {
            out.push(kept(&format!(
                "source gone, not replaced yet: '{unsettled}' of its series has no checked copy on the target"
            )));
        } else {
            out.push(Disposition::Prune { name });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sent,
    UpToDate,
    Warn,
    Fail,
    Pruned,
    Kept,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Self::Sent => "SENT",
            Self::UpToDate => "UP TO DATE",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Pruned => "PRUNED",
            Self::Kept => "KEPT",
        }
    }
}

/// One table sent to one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableReport {
    pub head: String,
    pub lines: Vec<(Kind, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub tables: Vec<TableReport>,
}

impl Outcome {
    /// A send, a check or a delete failed, or a table was refused.
    pub fn failed(&self) -> bool {
        self.tables
            .iter()
            .flat_map(|t| &t.lines)
            .any(|(k, _)| *k == Kind::Fail)
    }

    /// The `STATIC SUBVOLUMES` section; empty when no table is configured.
    pub fn section(&self, dry_run: bool) -> String {
        if self.tables.is_empty() {
            return String::new();
        }
        let mut r = String::from("STATIC SUBVOLUMES\n");
        if dry_run {
            r.push_str("  DRY RUN: nothing is sent, written or deleted\n");
        }
        for table in &self.tables {
            r.push_str(&format!("  {}\n", table.head));
            if table.lines.is_empty() {
                r.push_str("    no candidates\n");
            }
            for (kind, text) in &table.lines {
                r.push_str(&format!("    {:<10} {text}\n", kind.word()));
            }
        }
        r
    }

    /// The failing lines, for a run's error list.
    pub fn failures(&self) -> Vec<String> {
        self.tables
            .iter()
            .flat_map(|t| {
                t.lines
                    .iter()
                    .filter(|(k, _)| *k == Kind::Fail)
                    .map(move |(_, text)| format!("{}: {text}", t.head))
            })
            .collect()
    }
}

/// How a run carries the pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StaticStep {
    /// Not reached: the run was cut short, or it sends nothing.
    #[default]
    NotRun,
    Ran {
        failed: bool,
        /// The `STATIC SUBVOLUMES` section, empty with no table configured.
        section: String,
        failures: Vec<String>,
    },
}

impl StaticStep {
    pub fn from_outcome(outcome: &Outcome, dry_run: bool) -> Self {
        Self::Ran {
            failed: outcome.failed(),
            section: outcome.section(dry_run),
            failures: outcome.failures(),
        }
    }

    pub fn failed(&self) -> bool {
        matches!(self, Self::Ran { failed: true, .. })
    }

    /// The report's `Static subvolumes` cell.
    pub fn row(&self) -> String {
        match self {
            Self::NotRun => "N/A  (n/a)".into(),
            Self::Ran { failed: true, .. } => "FAIL  (see STATIC SUBVOLUMES in the report)".into(),
            Self::Ran { section, .. } if section.is_empty() => "N/A  (none configured)".into(),
            Self::Ran { .. } => "OK  (n/a)".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

/// The write-target check the pass is given (see [`Env::verify`]).
pub type VerifyTargets<'a> = dyn Fn(&[Target], &[String]) -> Result<(), String> + 'a;

/// What the pass needs from the host besides its arguments.
pub struct Env<'a> {
    /// Runs every `btrfs`, `blkid` and `findmnt`.
    pub runner: &'a dyn CommandRunner,
    /// Whether a path is a mount point now.
    pub is_mountpoint: &'a dyn Fn(&Path) -> bool,
    /// `mount::verify_write_targets` for the targets in the first argument,
    /// all written to: `Err` unless each is a real mount point holding the
    /// filesystem it must.
    pub verify: &'a VerifyTargets<'a>,
}

/// Run every static table of the sources named in `sources` against the
/// primary targets named in `targets` (labels). `dry_run` plans and reports
/// and changes nothing: no send, no marker, no directory, no delete.
pub fn run(
    config: &Config,
    sources: &[String],
    targets: &[String],
    dry_run: bool,
    env: &Env,
) -> Outcome {
    let mut outcome = Outcome::default();
    for source in config
        .sources
        .iter()
        .filter(|s| sources.contains(&s.label) && !s.static_subvolumes.is_empty())
    {
        let primaries: Vec<&Target> = source_targets(config, source)
            .into_iter()
            .filter(|t| t.role == TargetRole::Primary)
            .collect();
        let selected: Vec<&Target> = primaries
            .iter()
            .copied()
            .filter(|t| targets.contains(&t.label))
            .collect();
        if primaries.is_empty() {
            outcome.tables.push(TableReport {
                head: format!("source '{}'", source.label),
                lines: vec![(
                    Kind::Warn,
                    "no primary target receives this source: its static subvolumes are sent nowhere"
                        .into(),
                )],
            });
            continue;
        }
        if selected.is_empty() {
            continue;
        }
        // The source side is read once, however many tables and targets.
        let listing = crate::adopt::verify_volume(
            env.runner,
            env.is_mountpoint,
            &source.volume,
            &source.device,
        )
        .and_then(|_| read_listing(env.runner, &source.volume));
        for table in &source.static_subvolumes {
            for target in &selected {
                outcome
                    .tables
                    .push(one_table(source, table, &listing, target, dry_run, env));
            }
        }
    }
    outcome
}

/// Every existing component of `subdir` under `mount` must be a real
/// directory: a symlink would take the writes and the deletes elsewhere.
/// A component that is not there yet is fine.
fn path_is_clean(mount: &Path, subdir: &str) -> Result<(), String> {
    let mut path = mount.to_path_buf();
    for component in subdir.split('/') {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{} is a symlink", path.display()));
            }
            Ok(m) if !m.is_dir() => {
                return Err(format!("{} is not a directory", path.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("{} could not be examined: {e}", path.display())),
        }
    }
    Ok(())
}

/// Whether `path` is a real directory (never a symlink to one).
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// `btrfs subvolume delete <path>`, the one delete of this module, on one
/// exact path that is a real directory now.
fn delete_subvolume(runner: &dyn CommandRunner, path: &Path) -> Result<(), String> {
    if !is_real_dir(path) {
        return Err(format!("{} is not a real directory", path.display()));
    }
    match runner.output(
        Command::new("btrfs")
            .args(["subvolume", "delete"])
            .arg(path),
    ) {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "could not delete {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("could not delete {}: {e}", path.display())),
    }
}

/// One table sent to one target, start to end.
fn one_table(
    source: &crate::config::Source,
    table: &StaticSubvolumes,
    listing: &Result<Vec<Subvol>, String>,
    target: &Target,
    dry_run: bool,
    env: &Env,
) -> TableReport {
    let mut rep = TableReport {
        head: format!(
            "source '{}' dir '{}' pattern '{}' -> target '{}' dir '{}'",
            source.label, table.dir, table.pattern, target.label, table.target_subdir
        ),
        lines: Vec::new(),
    };
    macro_rules! stop {
        ($($arg:tt)*) => {{
            rep.lines.push((Kind::Fail, format!($($arg)*)));
            return rep;
        }};
    }
    let src = match listing {
        Ok(rows) => rows,
        Err(why) => stop!("nothing done: the source could not be read: {why}"),
    };
    if let Err(why) = (env.verify)(
        std::slice::from_ref(target),
        std::slice::from_ref(&target.label),
    ) {
        stop!("nothing done: the target is not verified: {why}");
    }
    if let Err(why) = target_top_level(env.runner, &target.mount) {
        stop!("nothing done: {why}");
    }
    let mount = Path::new(&target.mount);
    if let Err(why) = path_is_clean(mount, &table.target_subdir) {
        stop!("nothing done: {why}");
    }
    let dir = mount.join(&table.target_subdir);
    let marker_dir = dir.join(MARKER_DIR);
    let mut rows = match read_listing(env.runner, &target.mount) {
        Ok(rows) => rows,
        Err(why) => stop!("nothing done: the target could not be read: {why}"),
    };

    // 1. The debris of this module's own interrupted receives.
    match clear_interrupted(table, &rows, &dir, &marker_dir, dry_run, env, &mut rep) {
        Ok(true) => {
            rows = match read_listing(env.runner, &target.mount) {
                Ok(rows) => rows,
                Err(why) => stop!("nothing more done: the target could not be read again: {why}"),
            };
        }
        Ok(false) => {}
        Err(why) => stop!("{why}"),
    }

    // 2. What each candidate needs.
    let mut attempted = false;
    for item in plan_sends(
        table,
        Path::new(&source.volume),
        src,
        &table.target_subdir,
        &rows,
    ) {
        match item {
            Item::NotReadOnly { name } => rep.lines.push((
                Kind::Warn,
                format!("{name}: not read-only on the source — not sent, and never made read-only"),
            )),
            Item::UpToDate { name, copy } => rep.lines.push((
                Kind::UpToDate,
                if copy == name {
                    name
                } else {
                    format!("{name} (the copy is named {copy})")
                },
            )),
            Item::Refused { name, why } => rep.lines.push((Kind::Fail, format!("{name}: {why}"))),
            Item::Send {
                name,
                identity,
                source_path,
                parent,
            } => {
                let how = match &parent {
                    Some((p, _)) => format!("incremental from {p}"),
                    None => "full send".to_string(),
                };
                if dry_run {
                    rep.lines
                        .push((Kind::Sent, format!("would send {name} ({how})")));
                    continue;
                }
                attempted = true;
                match send_one(
                    &name,
                    &identity,
                    &source_path,
                    parent.as_ref().map(|p| p.1.as_path()),
                    (&dir, &marker_dir),
                    mount,
                    &table.target_subdir,
                    env,
                ) {
                    Ok(note) => rep
                        .lines
                        .push((Kind::Sent, format!("{name} ({how}){note}"))),
                    Err(why) => rep.lines.push((Kind::Fail, format!("{name}: {why}"))),
                }
            }
        }
    }

    // 3. What the sends made of the target is read again; only that listing
    //    decides what is superseded.
    if attempted {
        rows = match read_listing(env.runner, &target.mount) {
            Ok(rows) => rows,
            Err(why) => stop!("nothing pruned: the target could not be read again: {why}"),
        };
    }
    let subdir = normalise_dir(&table.target_subdir);
    let entries: Vec<&Subvol> = rows
        .iter()
        .filter(|r| normalise_dir(r.parent_dir()) == subdir)
        .collect();
    let cands: Vec<(&str, &str)> = candidates(table, src)
        .into_iter()
        .map(|c| (c.name(), c.identity()))
        .collect();
    let mut verified_again = false;
    for disposition in plan_prunes(&table.pattern, src, &cands, &entries) {
        match disposition {
            Disposition::Kept { name, why } => {
                rep.lines.push((Kind::Kept, format!("{name}: {why}")));
            }
            Disposition::Prune { name } if dry_run => rep.lines.push((
                Kind::Pruned,
                format!(
                    "would delete {name}: its pristine is gone and its series has a current copy"
                ),
            )),
            Disposition::Prune { name } => {
                // Just before the first delete, the target is proven again.
                if !verified_again {
                    if let Err(why) = (env.verify)(
                        std::slice::from_ref(target),
                        std::slice::from_ref(&target.label),
                    ) {
                        stop!("nothing pruned: the target is no longer verified: {why}");
                    }
                    if let Err(why) = path_is_clean(mount, &table.target_subdir) {
                        stop!("nothing pruned: {why}");
                    }
                    verified_again = true;
                }
                match delete_subvolume(env.runner, &dir.join(&name)) {
                    Ok(()) => rep.lines.push((
                        Kind::Pruned,
                        format!("{name}: its pristine is gone and its series has a current copy"),
                    )),
                    Err(why) => rep.lines.push((Kind::Fail, format!("{name}: {why}"))),
                }
            }
        }
    }
    rep
}

/// The target must be mounted at its filesystem's top level, or listing it
/// and joining paths to its mount mean nothing.
fn target_top_level(runner: &dyn CommandRunner, mount: &str) -> Result<(), String> {
    let out = runner
        .output(Command::new("findmnt").args(["-n", "-o", "FSROOT", "--target", mount]))
        .map_err(|e| format!("findmnt could not be run: {e}"))?;
    if !out.status.success() {
        return Err(format!("findmnt could not read {mount}"));
    }
    match String::from_utf8_lossy(&out.stdout).trim() {
        "/" => Ok(()),
        other => Err(format!(
            "{mount} is mounted at '{other}', not at its filesystem's top level"
        )),
    }
}

/// Deal with the markers left in `<dir>/.incoming`: a marker is written
/// before a receive and removed once its copy is checked, so one still there
/// means the receive was cut short. Returns whether a subvolume was deleted
/// (the target is to be listed again).
fn clear_interrupted(
    table: &StaticSubvolumes,
    rows: &[Subvol],
    dir: &Path,
    marker_dir: &Path,
    dry_run: bool,
    env: &Env,
    rep: &mut TableReport,
) -> Result<bool, String> {
    let read = match std::fs::read_dir(marker_dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(format!(
                "nothing done: {} could not be read: {e}",
                marker_dir.display()
            ));
        }
    };
    let mut names = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| {
            format!(
                "nothing done: {} could not be read: {e}",
                marker_dir.display()
            )
        })?;
        match entry.file_name().into_string() {
            Ok(n) => names.push(n),
            Err(_) => {
                return Err(format!(
                    "nothing done: {} holds a name that is not UTF-8",
                    marker_dir.display()
                ));
            }
        }
    }
    names.sort();
    let subdir = normalise_dir(&table.target_subdir);
    let mut deleted = false;
    for name in names {
        let marker = marker_dir.join(&name);
        let kept = |rep: &mut TableReport, why: &str| {
            rep.lines
                .push((Kind::Kept, format!("{MARKER_DIR}/{name}: {why}")));
        };
        if !std::fs::symlink_metadata(&marker).is_ok_and(|m| m.is_file()) {
            kept(rep, "not a marker file — left as it is");
            continue;
        }
        let row = rows
            .iter()
            .find(|r| normalise_dir(r.parent_dir()) == subdir && r.name() == name);
        let remove_marker = |rep: &mut TableReport| {
            if dry_run {
                return;
            }
            if let Err(e) = std::fs::remove_file(&marker) {
                rep.lines.push((
                    Kind::Warn,
                    format!("{MARKER_DIR}/{name}: the marker could not be removed: {e}"),
                ));
            }
        };
        match row {
            None => remove_marker(rep),
            Some(r) if r.readonly && r.received_uuid.is_some() => remove_marker(rep),
            Some(r)
                if !r.readonly
                    && r.received_uuid.is_none()
                    && glob_match(&table.pattern, &name) =>
            {
                if dry_run {
                    rep.lines.push((
                        Kind::Pruned,
                        format!(
                            "would delete {name}: an interrupted receive (its marker is in {MARKER_DIR})"
                        ),
                    ));
                    continue;
                }
                match delete_subvolume(env.runner, &dir.join(&name)) {
                    Ok(()) => {
                        deleted = true;
                        rep.lines.push((
                            Kind::Pruned,
                            format!(
                                "{name}: the unfinished receive of an earlier pass (its marker was in {MARKER_DIR})"
                            ),
                        ));
                        remove_marker(rep);
                    }
                    Err(why) => rep.lines.push((Kind::Fail, format!("{name}: {why}"))),
                }
            }
            Some(_) => kept(
                rep,
                "a marker beside a subvolume that is not an unfinished receive — both left as they are",
            ),
        }
    }
    Ok(deleted)
}

/// Receive one candidate and check it; a note for the report when it landed.
/// Any failure leaves the marker (and whatever the receive made) in place for
/// the next pass to recognise.
#[allow(clippy::too_many_arguments)]
fn send_one(
    name: &str,
    identity: &str,
    source_path: &Path,
    parent: Option<&Path>,
    (dir, marker_dir): (&Path, &Path),
    mount: &Path,
    subdir: &str,
    env: &Env,
) -> Result<String, String> {
    path_is_clean(mount, subdir)?;
    let dest = dir.join(name);
    match std::fs::symlink_metadata(&dest) {
        Ok(_) => {
            return Err(format!(
                "the destination {} is occupied — nothing is received over it",
                dest.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{} could not be examined: {e}", dest.display())),
    }
    std::fs::create_dir_all(marker_dir)
        .map_err(|e| format!("{} could not be made: {e}", marker_dir.display()))?;
    let marker = marker_dir.join(name);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .map_err(|e| format!("the marker {} could not be made: {e}", marker.display()))?;
        writeln!(file, "receive in progress; source identity {identity}")
            .map_err(|e| format!("the marker {} could not be written: {e}", marker.display()))?;
    }
    let mut producer = Command::new("btrfs");
    producer.arg("send");
    if let Some(parent) = parent {
        producer.arg("-p").arg(parent);
    }
    producer.arg(source_path);
    let mut consumer = Command::new("btrfs");
    consumer.arg("receive").arg(dir);
    let piped = env
        .runner
        .pipe(&mut producer, &mut consumer)
        .map_err(|e| format!("the send could not be run: {e}"))?;
    let said = |o: &std::process::Output| String::from_utf8_lossy(&o.stderr).trim().to_string();
    // BOTH sides must have succeeded: a consumer's success after a failed
    // producer is a truncated stream that happened to end cleanly.
    if !piped.producer.status.success() {
        return Err(format!(
            "btrfs send failed ({}): {} — the receive is left for the next pass to clear",
            piped.producer.status,
            said(&piped.producer)
        ));
    }
    if !piped.consumer.status.success() {
        return Err(format!(
            "btrfs receive failed ({}): {} — left for the next pass to clear",
            piped.consumer.status,
            said(&piped.consumer)
        ));
    }
    let out = env
        .runner
        .output(Command::new("btrfs").args(["subvolume", "show"]).arg(&dest))
        .map_err(|e| format!("the received copy could not be checked: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "the received copy failed its check: {} — left as it is",
            said(&out)
        ));
    }
    let shown = parse_show(&String::from_utf8_lossy(&out.stdout))
        .map_err(|e| format!("the received copy failed its check: {e} — left as it is"))?;
    if shown.received_uuid.as_deref() != Some(identity) || !shown.readonly {
        return Err(format!(
            "the received copy failed its check (received UUID {:?}, expected {identity}; read-only: {}) — left as it is",
            shown.received_uuid, shown.readonly
        ));
    }
    Ok(match std::fs::remove_file(&marker) {
        Ok(()) => String::new(),
        Err(e) => format!(" — the marker could not be removed: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Source;
    use crate::fsutil::testing::Scripted;

    const U: [&str; 8] = [
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-4000-8000-000000000002",
        "00000000-0000-4000-8000-000000000003",
        "00000000-0000-4000-8000-000000000004",
        "00000000-0000-4000-8000-000000000005",
        "00000000-0000-4000-8000-000000000006",
        "00000000-0000-4000-8000-000000000007",
        "00000000-0000-4000-8000-000000000008",
    ];

    fn sv(
        id: u64,
        path: &str,
        uuid: &str,
        parent: Option<&str>,
        recv: Option<&str>,
        ro: bool,
    ) -> Subvol {
        Subvol {
            id,
            path: path.into(),
            uuid: uuid.into(),
            parent_uuid: parent.map(str::to_string),
            received_uuid: recv.map(str::to_string),
            readonly: ro,
        }
    }

    fn table() -> StaticSubvolumes {
        StaticSubvolumes {
            dir: ".snapshots".into(),
            pattern: "*.pristine-*".into(),
            target_subdir: "static/vm".into(),
        }
    }

    // ---- the identity rule, in one place --------------------------------

    #[test]
    fn identity_is_the_received_uuid_when_there_is_one_else_the_uuid() {
        let plain = sv(1, "a", U[0], None, None, true);
        assert_eq!(plain.identity(), U[0]);
        let restored = sv(2, "b", U[1], None, Some(U[2]), true);
        assert_eq!(restored.identity(), U[2]);
    }

    #[test]
    fn a_copy_belongs_to_the_candidate_whose_identity_it_carries_with_or_without_a_received_uuid() {
        let vol = Path::new("/v");
        // Candidate WITHOUT its own received_uuid: the copy carries its uuid.
        let src = vec![sv(10, ".snapshots/vm.pristine-1", U[0], None, None, true)];
        let tgt = vec![sv(
            5,
            "static/vm/vm.pristine-1",
            U[7],
            None,
            Some(U[0]),
            true,
        )];
        assert!(matches!(
            plan_sends(&table(), vol, &src, "static/vm", &tgt)[..],
            [Item::UpToDate { .. }]
        ));
        // Candidate WITH a received_uuid (a restored pristine): the copy
        // carries THAT, not the candidate's own uuid.
        let src = vec![sv(
            10,
            ".snapshots/vm.pristine-1",
            U[0],
            None,
            Some(U[3]),
            true,
        )];
        let tgt = vec![sv(
            5,
            "static/vm/vm.pristine-1",
            U[7],
            None,
            Some(U[3]),
            true,
        )];
        assert!(matches!(
            plan_sends(&table(), vol, &src, "static/vm", &tgt)[..],
            [Item::UpToDate { .. }]
        ));
        // A copy carrying the candidate's own uuid is NOT its copy then.
        let tgt = vec![sv(
            5,
            "static/vm/vm.pristine-1",
            U[7],
            None,
            Some(U[0]),
            true,
        )];
        assert!(matches!(
            plan_sends(&table(), vol, &src, "static/vm", &tgt)[..],
            [Item::Refused { .. }]
        ));
    }

    // ---- listings -----------------------------------------------------------

    const ROW: &str = "ID 257 gen 9 top level 5 parent_uuid fb91a62b-e8c0-f149-9c35-d46b80a88c80 received_uuid -                                    uuid 9633238c-6d1f-f547-a20a-d22ce5cb78da path .snapshots/vm.pristine-1";

    #[test]
    fn a_listing_row_is_read_by_the_names_of_its_fields_and_a_path_may_hold_spaces() {
        let rows = parse_rows(&format!("{ROW}\nWARNING: x path y\n")).unwrap();
        assert_eq!(
            rows,
            vec![(
                257,
                ".snapshots/vm.pristine-1".to_string(),
                "9633238c-6d1f-f547-a20a-d22ce5cb78da".to_string(),
                Some("fb91a62b-e8c0-f149-9c35-d46b80a88c80".to_string()),
                None
            )]
        );
        let spaced = ROW.replace("vm.pristine-1", "a b uuid c");
        assert_eq!(parse_rows(&spaced).unwrap()[0].1, ".snapshots/a b uuid c");
    }

    #[test]
    fn a_listing_row_that_does_not_read_completely_is_an_error_not_a_skip() {
        for bad in [
            ROW.replace(" uuid 9633238c-6d1f-f547-a20a-d22ce5cb78da", " uuid -"),
            ROW.replace("uuid 9633238c-6d1f-f547-a20a-d22ce5cb78da", "uuid nonsense"),
            ROW.replace("received_uuid -", "received_uuid nonsense"),
            ROW.replace("ID 257", "ID x"),
            ROW.replace(" path .snapshots/vm.pristine-1", " path "),
            ROW.replace("parent_uuid", "parent"),
        ] {
            assert!(parse_rows(&bad).is_err(), "{bad}");
        }
        assert!(
            parse_rows(&format!("{ROW}\n{ROW}"))
                .unwrap_err()
                .contains("twice")
        );
        // A line that is not a row is not one.
        assert_eq!(parse_rows("Usage: btrfs\n").unwrap(), vec![]);
    }

    fn scripted_listing(mount: &str, all: &str, ro: &str) -> Scripted {
        Scripted::new(&[
            (&format!("btrfs subvolume list -q -u -R {mount}"), 0, all),
            (&format!("btrfs subvolume list -q -u -R -r {mount}"), 0, ro),
        ])
    }

    #[test]
    fn read_listing_marks_exactly_the_read_only_ids_and_refuses_what_it_cannot_trust() {
        let other = ROW
            .replace("ID 257", "ID 258")
            .replace("vm.pristine-1", "vm.pristine-2")
            .replace(
                "9633238c-6d1f-f547-a20a-d22ce5cb78da",
                "9633238c-6d1f-f547-a20a-d22ce5cb78db",
            );
        let all = format!("{ROW}\n{other}\n");
        let rows = read_listing(&scripted_listing("/m", &all, &format!("{ROW}\n")), "/m").unwrap();
        assert_eq!((rows[0].readonly, rows[1].readonly), (true, false));
        // Empty is unreadable, never "nothing here".
        assert!(
            read_listing(&scripted_listing("/m", "", ""), "/m")
                .unwrap_err()
                .contains("unreadable")
        );
        // A read-only id the full listing lacks.
        assert!(
            read_listing(
                &scripted_listing("/m", &format!("{ROW}\n"), &format!("{other}\n")),
                "/m"
            )
            .is_err()
        );
        // Either call failing.
        let half = Scripted::new(&[("btrfs subvolume list -q -u -R /m", 0, ROW)]);
        assert!(read_listing(&half, "/m").is_err());
        assert!(read_listing(&Scripted::new(&[]), "/m").is_err());
    }

    #[test]
    fn show_is_read_by_label_and_a_missing_label_is_an_error() {
        let text = "static/x\n\tName: \t\t\tx\n\tUUID: \t\t\t63d7331e-1758-a74c-a7a6-da65eeaca50a\n\tParent UUID: \t\t-\n\tReceived UUID: \t\td4c3637a-dffe-e140-a7b7-daaa3b200df1\n\tFlags: \t\t\treadonly\n";
        let shown = parse_show(text).unwrap();
        assert_eq!(shown.uuid, "63d7331e-1758-a74c-a7a6-da65eeaca50a");
        assert_eq!(
            shown.received_uuid.as_deref(),
            Some("d4c3637a-dffe-e140-a7b7-daaa3b200df1")
        );
        assert!(shown.readonly);
        let partial = text
            .replace("d4c3637a-dffe-e140-a7b7-daaa3b200df1", "-")
            .replace("readonly", "-");
        let shown = parse_show(&partial).unwrap();
        assert_eq!((shown.received_uuid, shown.readonly), (None, false));
        assert!(parse_show(&text.replace("\tFlags: \t\t\treadonly\n", "")).is_err());
        assert!(
            parse_show(&text.replace(
                "\tReceived UUID: \t\td4c3637a-dffe-e140-a7b7-daaa3b200df1\n",
                ""
            ))
            .is_err()
        );
        assert!(parse_show("").is_err());
    }

    // ---- globs and series ---------------------------------------------------

    #[test]
    fn glob_matches_star_and_question_mark_on_the_whole_name_only() {
        assert!(glob_match("*.pristine-*", "dev-vm.pristine-2026-10-21"));
        assert!(glob_match("*.pristine-*", "x.pristine-"));
        assert!(!glob_match("*.pristine-*", "x.pristine"));
        assert!(glob_match("a?c", "abc") && !glob_match("a?c", "ac") && !glob_match("a?c", "abbc"));
        assert!(glob_match("a*", "a") && !glob_match("a*", "ba"));
        // Everything else is literal.
        assert!(!glob_match("a[bc]", "ab") && glob_match("a[bc]", "a[bc]"));
        assert!(glob_match("é*", "éa") && glob_match("?", "é"));
    }

    #[test]
    fn a_series_is_the_name_up_to_what_the_final_star_matched() {
        let p = "*.pristine-*";
        assert_eq!(
            series_of(p, "dev-vm.pristine-2026-10-21"),
            Some("dev-vm.pristine-")
        );
        assert_eq!(
            series_of(p, "dev-vm.pristine-2026-10-21-tag.pristine-x"),
            Some("dev-vm.pristine-")
        );
        assert_eq!(
            series_of(p, "qa-vm.pristine-2026-10-21"),
            Some("qa-vm.pristine-")
        );
        assert_eq!(series_of("vm-?-*", "vm-a-2026"), Some("vm-a-"));
        // None: no match, no final star, no literal before it.
        assert_eq!(series_of(p, "dev-vm.golden"), None);
        assert_eq!(series_of("*.pristine-2026", "x.pristine-2026"), None);
        assert_eq!(series_of("*", "x"), None);
        assert_eq!(series_of("?*", "x"), None);
        assert_eq!(series_of("**", "x"), None);
    }

    // ---- the plan of sends ----------------------------------------------------

    #[test]
    fn plan_sends_leaves_a_read_write_candidate_alone_and_only_in_the_directory_and_pattern() {
        let vol = Path::new("/v");
        let src = vec![
            sv(1, ".snapshots/vm.pristine-1", U[0], None, None, false),
            sv(2, ".snapshots/other", U[1], None, None, true),
            sv(3, "deeper/.snapshots/vm.pristine-2", U[2], None, None, true),
            sv(4, ".snapshots/sub/vm.pristine-3", U[3], None, None, true),
        ];
        let tgt = vec![sv(9, "static/x", U[4], None, None, true)];
        assert_eq!(
            plan_sends(&table(), vol, &src, "static/vm", &tgt),
            vec![Item::NotReadOnly {
                name: "vm.pristine-1".into()
            }]
        );
    }

    #[test]
    fn plan_sends_refuses_a_name_taken_by_anything_else_and_a_non_read_only_copy() {
        let vol = Path::new("/v");
        let src = vec![sv(1, ".snapshots/vm.pristine-1", U[0], None, None, true)];
        for taken in [
            sv(9, "static/vm/vm.pristine-1", U[4], None, Some(U[5]), true), // another's copy
            sv(9, "static/vm/vm.pristine-1", U[4], None, None, false),      // unfinished
            sv(9, "static/vm/vm.pristine-1", U[4], None, None, true),       // not received
        ] {
            let plan = plan_sends(&table(), vol, &src, "static/vm", &[taken]);
            assert!(matches!(plan[..], [Item::Refused { .. }]), "{plan:?}");
        }
        // A copy of it that is read-write is not up to date and not resent.
        let rw = sv(9, "static/vm/elsewhere", U[4], None, Some(U[0]), false);
        let plan = plan_sends(&table(), vol, &src, "static/vm", &[rw]);
        assert!(matches!(plan[..], [Item::Refused { .. }]), "{plan:?}");
        // The same name in ANOTHER directory of the target is no conflict.
        let far = sv(9, "elsewhere/vm.pristine-1", U[4], None, Some(U[5]), true);
        let plan = plan_sends(&table(), vol, &src, "static/vm", &[far]);
        assert!(
            matches!(plan[..], [Item::Send { parent: None, .. }]),
            "{plan:?}"
        );
    }

    #[test]
    fn plan_sends_picks_the_newest_read_only_sibling_the_target_holds_as_the_parent() {
        let vol = Path::new("/v");
        let vm = Some(U[7]);
        let src = vec![
            sv(1, "vm", U[7], None, None, false),
            sv(5, ".btrbk-snapshots/vm.1", U[0], vm, None, true),
            sv(6, ".btrbk-snapshots/vm.2", U[1], vm, None, true),
            sv(7, ".btrbk-snapshots/vm.3", U[2], vm, None, true), // newest, target lacks it
            sv(8, ".btrbk-snapshots/vm.rw", U[3], vm, None, false), // not read-only
            sv(9, ".btrbk-snapshots/other", U[4], Some(U[6]), None, true), // another VM
            sv(20, ".snapshots/vm.pristine-9", U[5], vm, None, true), // the candidate
        ];
        let tgt = vec![
            sv(100, "vm/vm.1", U[6], None, Some(U[0]), true),
            sv(101, "vm/vm.2", U[6], None, Some(U[1]), true),
            sv(102, "vm/vm.rw", U[6], None, Some(U[3]), true),
            sv(103, "vm/other", U[6], None, Some(U[4]), true),
        ];
        let plan = plan_sends(&table(), vol, &src, "static/vm", &tgt);
        let Item::Send {
            parent,
            source_path,
            identity,
            ..
        } = &plan[0]
        else {
            panic!("{plan:?}")
        };
        assert_eq!(identity, U[5]);
        assert_eq!(source_path, Path::new("/v/.snapshots/vm.pristine-9"));
        // vm.rw is not read-only on the source, vm.3 is not on the target:
        // the newest that is both is vm.2.
        assert_eq!(parent.as_ref().map(|p| p.0.as_str()), Some("vm.2"));
        assert_eq!(
            parent.as_ref().unwrap().1,
            Path::new("/v/.btrbk-snapshots/vm.2")
        );
        // A target copy by the sibling's own uuid (not its identity) or a
        // read-write one is not a copy of it: a full send.
        let tgt = vec![
            sv(100, "vm/vm.1", U[6], None, Some(U[7]), true),
            sv(101, "vm/vm.2", U[6], None, Some(U[1]), false),
        ];
        assert!(matches!(
            plan_sends(&table(), vol, &src, "static/vm", &tgt)[0],
            Item::Send { parent: None, .. }
        ));
        // The identity of a sibling that was itself received is its received uuid.
        let src2 = vec![
            sv(5, ".btrbk-snapshots/vm.1", U[0], vm, Some(U[3]), true),
            sv(20, ".snapshots/vm.pristine-9", U[5], vm, None, true),
        ];
        let tgt = vec![sv(100, "vm/vm.1", U[6], None, Some(U[3]), true)];
        let plan = plan_sends(&table(), vol, &src2, "static/vm", &tgt);
        assert!(
            matches!(&plan[0], Item::Send { parent: Some((n, _)), .. } if n == "vm.1"),
            "{plan:?}"
        );
        // No parent_uuid on the candidate: always a full send.
        let src3 = vec![sv(20, ".snapshots/vm.pristine-9", U[5], None, None, true)];
        assert!(matches!(
            plan_sends(&table(), vol, &src3, "static/vm", &tgt)[0],
            Item::Send { parent: None, .. }
        ));
    }

    // ---- the plan of prunes ----------------------------------------------------

    fn prune_case(src: &[Subvol], tgt: &[Subvol], pattern: &str) -> Vec<Disposition> {
        let t = StaticSubvolumes {
            pattern: pattern.into(),
            ..table()
        };
        let cands: Vec<(&str, &str)> = candidates(&t, src)
            .iter()
            .map(|c| (c.name(), c.identity()))
            .collect();
        let entries: Vec<&Subvol> = tgt.iter().collect();
        plan_prunes(pattern, src, &cands, &entries)
    }

    fn pruned(d: &[Disposition]) -> Vec<&str> {
        d.iter()
            .filter_map(|d| match d {
                Disposition::Prune { name } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_gone_copy_is_pruned_only_once_a_candidate_of_its_series_has_a_checked_copy() {
        let src = vec![sv(1, ".snapshots/vm.pristine-2", U[1], None, None, true)];
        let old = sv(10, "static/vm/vm.pristine-1", U[6], None, Some(U[0]), true);
        let new = sv(11, "static/vm/vm.pristine-2", U[6], None, Some(U[1]), true);
        // Replaced: the old one goes, the current one stays.
        let d = prune_case(&src, &[old.clone(), new.clone()], "*.pristine-*");
        assert_eq!(pruned(&d), ["vm.pristine-1"], "{d:?}");
        // Not landed yet: kept, and says why.
        let d = prune_case(&src, std::slice::from_ref(&old), "*.pristine-*");
        assert!(pruned(&d).is_empty());
        assert!(
            matches!(&d[0], Disposition::Kept { why, .. } if why.contains("no checked copy")),
            "{d:?}"
        );
        // The copy of the current one is not read-only: not checked, kept.
        let mut rw = new.clone();
        rw.readonly = false;
        assert!(pruned(&prune_case(&src, &[old.clone(), rw], "*.pristine-*")).is_empty());
    }

    #[test]
    fn a_gone_copy_with_no_candidate_at_all_or_of_another_series_is_kept() {
        let old = sv(10, "static/vm/vm.pristine-1", U[6], None, Some(U[0]), true);
        // The pristine was deleted by accident: nothing replaces it.
        let d = prune_case(&[], std::slice::from_ref(&old), "*.pristine-*");
        assert!(
            matches!(&d[0], Disposition::Kept { why, .. } if why.contains("no candidate")),
            "{d:?}"
        );
        // Another VM's pristine is current; this VM's is not replaced.
        let src = vec![sv(1, ".snapshots/qa.pristine-1", U[1], None, None, true)];
        let other = sv(11, "static/vm/qa.pristine-1", U[6], None, Some(U[1]), true);
        let d = prune_case(&src, &[old, other], "*.pristine-*");
        assert!(pruned(&d).is_empty(), "{d:?}");
    }

    #[test]
    fn a_series_with_one_unsettled_candidate_prunes_nothing() {
        let src = vec![
            sv(1, ".snapshots/vm.pristine-2", U[1], None, None, true),
            sv(2, ".snapshots/vm.pristine-3", U[2], None, None, true), // no copy: its send failed
            sv(3, ".snapshots/vm.pristine-4", U[3], None, None, false), // read-write
        ];
        let old = sv(10, "static/vm/vm.pristine-1", U[6], None, Some(U[0]), true);
        let new = sv(11, "static/vm/vm.pristine-2", U[6], None, Some(U[1]), true);
        let d = prune_case(&src, &[old, new], "*.pristine-*");
        assert!(pruned(&d).is_empty(), "{d:?}");
    }

    #[test]
    fn a_copy_whose_source_is_present_anywhere_on_the_volume_is_never_gone() {
        let old = sv(10, "static/vm/vm.pristine-1", U[6], None, Some(U[0]), true);
        let new = sv(11, "static/vm/vm.pristine-2", U[6], None, Some(U[1]), true);
        // Its source moved out of the directory: still on the volume.
        let src = vec![
            sv(1, ".snapshots/vm.pristine-2", U[1], None, None, true),
            sv(2, "elsewhere/renamed", U[0], None, None, true),
        ];
        assert!(
            pruned(&prune_case(
                &src,
                &[old.clone(), new.clone()],
                "*.pristine-*"
            ))
            .is_empty()
        );
        // Its source is a restored subvolume carrying the identity as received_uuid.
        let src = vec![
            sv(1, ".snapshots/vm.pristine-2", U[1], None, None, true),
            sv(2, "elsewhere/renamed", U[5], None, Some(U[0]), true),
        ];
        assert!(pruned(&prune_case(&src, &[old, new], "*.pristine-*")).is_empty());
    }

    #[test]
    fn a_pattern_that_names_no_series_never_prunes_and_a_foreign_subvolume_is_never_deleted() {
        let src = vec![sv(1, ".snapshots/vm.pristine-2", U[1], None, None, true)];
        let old = sv(10, "static/vm/vm.pristine-1", U[6], None, Some(U[0]), true);
        let new = sv(11, "static/vm/vm.pristine-2", U[6], None, Some(U[1]), true);
        for pattern in ["*.pristine-2", "*"] {
            assert!(
                pruned(&prune_case(&src, &[old.clone(), new.clone()], pattern)).is_empty(),
                "{pattern}"
            );
        }
        let rw = sv(12, "static/vm/handmade", U[6], None, None, false);
        let ro_plain = sv(13, "static/vm/plain", U[6], None, None, true);
        let d = prune_case(&src, &[rw, ro_plain, new], "*.pristine-*");
        assert!(pruned(&d).is_empty(), "{d:?}");
        assert_eq!(d.len(), 2, "both foreign ones are listed: {d:?}");
    }

    #[test]
    fn the_section_names_every_line_and_only_a_fail_fails_the_run() {
        let outcome = Outcome {
            tables: vec![TableReport {
                head: "s .snapshots -> t:static".into(),
                lines: vec![(Kind::Sent, "a".into()), (Kind::Kept, "b".into())],
            }],
        };
        assert!(!outcome.failed());
        let text = outcome.section(true);
        assert!(text.starts_with("STATIC SUBVOLUMES\n  DRY RUN"), "{text}");
        assert!(
            text.contains("SENT       a") && text.contains("KEPT       b"),
            "{text}"
        );
        let mut failed = outcome.clone();
        failed.tables[0].lines.push((Kind::Fail, "boom".into()));
        assert!(failed.failed());
        assert_eq!(failed.failures(), ["s .snapshots -> t:static: boom"]);
        assert_eq!(Outcome::default().section(false), "");
        assert_eq!(
            StaticStep::from_outcome(&Outcome::default(), false).row(),
            "N/A  (none configured)"
        );
        assert_eq!(StaticStep::from_outcome(&outcome, false).row(), "OK  (n/a)");
        assert!(StaticStep::from_outcome(&failed, false).failed());
    }

    // ---- the pass, through the command seam ---------------------------------------

    use crate::config::Retention;
    use std::cell::Cell;

    fn line(id: u64, path: &str, uuid: &str, parent: Option<&str>, recv: Option<&str>) -> String {
        format!(
            "ID {id} gen 9 top level 5 parent_uuid {} received_uuid {} uuid {uuid} path {path}\n",
            parent.unwrap_or("-"),
            recv.unwrap_or("-")
        )
    }

    /// A target mount that is a real directory, a source volume `/v` that is
    /// not (only its name is used), and the answers `btrfs` gives.
    struct Fx {
        dir: tempfile::TempDir,
        cfg: Config,
        answers: Vec<(String, i32, String)>,
        pipes: Vec<(String, String, i32, i32, Option<String>)>,
    }

    const PRISTINE: &str = ".snapshots/vm.pristine-2";

    impl Fx {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mount = dir.path().to_string_lossy().into_owned();
            let target = |label: &str, mount: &str, role| Target {
                label: label.into(),
                serial: "S".into(),
                serials: vec!["S".into()],
                mount_uuid: None,
                mount: mount.into(),
                role,
                retention: Retention {
                    daily: 7,
                    ..Default::default()
                },
                display_name: String::new(),
                esp_partuuid: None,
            };
            let mut cfg = Config::default();
            cfg.sources.push(Source {
                label: "s".into(),
                volume: "/v".into(),
                subvolumes: vec![],
                device: "UUID=su".into(),
                snapshot_dir: ".btrbk-snapshots".into(),
                target_subdirs: vec![],
                target_labels: vec![],
                static_subvolumes: vec![table()],
            });
            cfg.targets.push(target("t", &mount, TargetRole::Primary));
            cfg.targets
                .push(target("m", "/nonexistent-mirror", TargetRole::Mirror));
            let mut fx = Self {
                dir,
                cfg,
                answers: vec![],
                pipes: vec![],
            };
            fx.say("findmnt -n -o UUID,FSROOT --target /v", "su /\n");
            fx.say(&format!("findmnt -n -o FSROOT --target {mount}"), "/\n");
            fx
        }
        fn mount(&self) -> String {
            self.dir.path().to_string_lossy().into_owned()
        }
        fn say(&mut self, argv: &str, out: &str) {
            self.answers.push((argv.into(), 0, out.into()));
        }
        /// A listing of `path`: all rows, then which of them are read-only.
        fn list(&mut self, path: &str, all: &[String], ro: &[String]) {
            self.say(
                &format!("btrfs subvolume list -q -u -R {path}"),
                &all.concat(),
            );
            self.say(
                &format!("btrfs subvolume list -q -u -R -r {path}"),
                &ro.concat(),
            );
        }
        fn show(&mut self, name: &str, recv: Option<&str>, ro: bool) {
            let path = format!("{}/static/vm/{name}", self.mount());
            self.say(
                &format!("btrfs subvolume show {path}"),
                &format!(
                    "x\n\tUUID: \t\t\t{}\n\tReceived UUID: \t\t{}\n\tFlags: \t\t\t{}\n",
                    U[7],
                    recv.unwrap_or("-"),
                    if ro { "readonly" } else { "-" }
                ),
            );
        }
        fn pipe(&mut self, send: &str, recv_ok: (i32, i32), creates: Option<&str>) {
            let m = self.mount();
            self.pipes.push((
                format!("btrfs {send}"),
                format!("btrfs receive {m}/static/vm"),
                recv_ok.0,
                recv_ok.1,
                creates.map(|c| format!("{m}/static/vm/{c}")),
            ));
        }
        fn runner(&self) -> Scripted {
            let mut r = Scripted::from_owned(self.answers.clone()).sequenced();
            for (p, c, pc, cc, d) in &self.pipes {
                r = r.piping((p, c), (*pc, *cc), d.as_deref());
            }
            r
        }
        fn go(&self, runner: &Scripted, dry: bool) -> Outcome {
            self.go_with(runner, dry, &|_, _| Ok(()))
        }
        fn go_with(&self, runner: &Scripted, dry: bool, verify: &VerifyTargets) -> Outcome {
            let env = Env {
                runner,
                is_mountpoint: &|_| true,
                verify,
            };
            run(
                &self.cfg,
                &["s".into()],
                &["t".into(), "m".into()],
                dry,
                &env,
            )
        }
        fn marker(&self, name: &str) -> PathBuf {
            self.dir
                .path()
                .join("static/vm")
                .join(MARKER_DIR)
                .join(name)
        }
        fn make_dir(&self, rel: &str) -> PathBuf {
            let p = self.dir.path().join(rel);
            std::fs::create_dir_all(&p).unwrap();
            p
        }
    }

    fn kinds(o: &Outcome) -> Vec<(Kind, String)> {
        o.tables.iter().flat_map(|t| t.lines.clone()).collect()
    }
    fn has(o: &Outcome, kind: Kind, needle: &str) -> bool {
        kinds(o)
            .iter()
            .any(|(k, t)| *k == kind && t.contains(needle))
    }
    fn deletes(r: &Scripted) -> Vec<String> {
        r.calls()
            .into_iter()
            .filter(|c| c.contains("subvolume delete"))
            .collect()
    }
    fn pipes(r: &Scripted) -> Vec<String> {
        r.calls()
            .into_iter()
            .filter(|c| c.contains(" | "))
            .collect()
    }

    /// Source: the VM, a btrbk snapshot of it the target holds, and the new
    /// pristine. Target: that btrbk snapshot's copy.
    fn standard_source(fx: &mut Fx, extra: &[String]) {
        let mut src = vec![
            line(1, "vm", U[7], None, None),
            line(5, ".btrbk-snapshots/vm.1", U[0], Some(U[7]), None),
            line(20, PRISTINE, U[1], Some(U[7]), None),
        ];
        src.extend_from_slice(extra);
        let ro: Vec<String> = src.iter().skip(1).cloned().collect();
        fx.list("/v", &src, &ro);
    }

    fn standard(fx: &mut Fx, target_extra: &[String], target_ro_extra: &[String]) {
        standard_source(fx, &[]);
        let mut tgt = vec![line(100, "vm/vm.1", U[6], None, Some(U[0]))];
        let mut tro = tgt.clone();
        tgt.extend_from_slice(target_extra);
        tro.extend_from_slice(target_ro_extra);
        let m = fx.mount();
        fx.list(&m, &tgt, &tro);
    }

    #[test]
    fn a_new_pristine_is_sent_incrementally_received_checked_and_its_marker_removed() {
        let mut fx = Fx::new();
        standard(&mut fx, &[], &[]);
        let m = fx.mount();
        fx.pipe(
            &format!("send -p /v/.btrbk-snapshots/vm.1 /v/{PRISTINE}"),
            (0, 0),
            Some("vm.pristine-2"),
        );
        fx.show("vm.pristine-2", Some(U[1]), true);
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(!o.failed(), "{}", o.section(false));
        assert!(has(&o, Kind::Sent, "vm.pristine-2"), "{}", o.section(false));
        assert!(
            has(&o, Kind::Sent, "incremental from vm.1"),
            "{}",
            o.section(false)
        );
        assert_eq!(
            pipes(&r),
            [format!(
                "btrfs send -p /v/.btrbk-snapshots/vm.1 /v/{PRISTINE} | btrfs receive {m}/static/vm"
            )]
        );
        assert!(
            !fx.marker("vm.pristine-2").exists(),
            "the marker goes once the copy is checked"
        );
        assert!(fx.dir.path().join("static/vm").join(MARKER_DIR).is_dir());
        assert!(deletes(&r).is_empty());
        // Nothing in the mirror's name was ever asked.
        assert!(r.calls().iter().all(|c| !c.contains("nonexistent-mirror")));
    }

    #[test]
    fn with_no_sibling_on_the_target_it_is_a_full_send() {
        let mut fx = Fx::new();
        standard_source(&mut fx, &[]);
        let m = fx.mount();
        let other = [line(100, "vm/other", U[6], None, Some(U[5]))];
        fx.list(&m, &other, &other);
        fx.pipe(
            &format!("send /v/{PRISTINE}"),
            (0, 0),
            Some("vm.pristine-2"),
        );
        fx.show("vm.pristine-2", Some(U[1]), true);
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(!o.failed(), "{}", o.section(false));
        assert!(has(&o, Kind::Sent, "full send"), "{}", o.section(false));
    }

    #[test]
    fn what_is_on_the_target_already_is_up_to_date_and_nothing_runs() {
        let mut fx = Fx::new();
        let copy = line(101, "static/vm/vm.pristine-2", U[6], None, Some(U[1]));
        standard(
            &mut fx,
            std::slice::from_ref(&copy),
            std::slice::from_ref(&copy),
        );
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(
            has(&o, Kind::UpToDate, "vm.pristine-2"),
            "{}",
            o.section(false)
        );
        assert!(!o.failed());
        assert!(pipes(&r).is_empty() && deletes(&r).is_empty());
        assert!(
            !fx.dir.path().join("static").exists(),
            "nothing to write, nothing created"
        );
    }

    #[test]
    fn a_dry_run_plans_and_changes_nothing() {
        let mut fx = Fx::new();
        let old = line(101, "static/vm/vm.pristine-1", U[6], None, Some(U[3]));
        let new = line(102, "static/vm/vm.pristine-2", U[6], None, Some(U[1]));
        standard(&mut fx, &[old.clone(), new.clone()], &[old, new]);
        fx.make_dir("static/vm/vm.pristine-1");
        let r = fx.runner();
        let o = fx.go(&r, true);
        assert!(has(&o, Kind::Pruned, "would delete"), "{}", o.section(true));
        assert!(o.section(true).contains("DRY RUN"));
        assert!(pipes(&r).is_empty() && deletes(&r).is_empty());
        assert!(!fx.dir.path().join("static/vm").join(MARKER_DIR).exists());
        // A send that would happen is said, not done.
        let mut fx = Fx::new();
        standard(&mut fx, &[], &[]);
        let r = fx.runner();
        let o = fx.go(&r, true);
        assert!(
            has(&o, Kind::Sent, "would send vm.pristine-2"),
            "{}",
            o.section(true)
        );
        assert!(pipes(&r).is_empty() && !fx.dir.path().join("static").exists());
    }

    #[test]
    fn a_failed_producer_beside_a_successful_receive_is_a_failure_and_keeps_its_marker() {
        let mut fx = Fx::new();
        standard(&mut fx, &[], &[]);
        fx.pipe(
            &format!("send -p /v/.btrbk-snapshots/vm.1 /v/{PRISTINE}"),
            (1, 0),
            Some("vm.pristine-2"),
        );
        fx.show("vm.pristine-2", Some(U[1]), true);
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(has(&o, Kind::Fail, "vm.pristine-2"), "{}", o.section(false));
        assert!(
            fx.marker("vm.pristine-2").exists(),
            "the next pass needs it to recognise its own debris"
        );
        assert!(
            deletes(&r).is_empty(),
            "a failed send deletes nothing in the pass"
        );
        assert!(
            r.calls().iter().all(|c| !c.contains("subvolume show")),
            "an unfinished send is not checked as a copy"
        );
    }

    #[test]
    fn a_failed_receive_is_a_failure_and_one_failure_does_not_stop_the_next_send() {
        let mut fx = Fx::new();
        standard_source(
            &mut fx,
            &[line(21, ".snapshots/vm.pristine-3", U[2], Some(U[7]), None)],
        );
        let m = fx.mount();
        let base = [line(100, "vm/vm.1", U[6], None, Some(U[0]))];
        fx.list(&m, &base, &base);
        fx.pipe(
            &format!("send -p /v/.btrbk-snapshots/vm.1 /v/{PRISTINE}"),
            (0, 1),
            None,
        );
        fx.pipe(
            "send -p /v/.btrbk-snapshots/vm.1 /v/.snapshots/vm.pristine-3",
            (0, 0),
            Some("vm.pristine-3"),
        );
        fx.show("vm.pristine-3", Some(U[2]), true);
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(has(&o, Kind::Fail, "vm.pristine-2"), "{}", o.section(false));
        assert!(has(&o, Kind::Sent, "vm.pristine-3"), "{}", o.section(false));
        assert!(o.failed());
    }

    #[test]
    fn a_received_copy_that_fails_its_check_is_a_failure_and_is_left_alone() {
        for (recv, ro) in [(Some(U[4]), true), (None, true), (Some(U[1]), false)] {
            let mut fx = Fx::new();
            standard(&mut fx, &[], &[]);
            fx.pipe(
                &format!("send -p /v/.btrbk-snapshots/vm.1 /v/{PRISTINE}"),
                (0, 0),
                Some("vm.pristine-2"),
            );
            fx.show("vm.pristine-2", recv, ro);
            let r = fx.runner();
            let o = fx.go(&r, false);
            assert!(
                has(&o, Kind::Fail, "failed its check"),
                "{recv:?} {ro}: {}",
                o.section(false)
            );
            assert!(fx.marker("vm.pristine-2").exists());
            assert!(deletes(&r).is_empty());
        }
    }

    #[test]
    fn a_destination_that_exists_on_disk_is_never_received_into_even_when_the_listing_lacks_it() {
        let mut fx = Fx::new();
        standard(&mut fx, &[], &[]);
        fx.make_dir("static/vm/vm.pristine-2"); // an empty directory a rename would replace
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(has(&o, Kind::Fail, "occupied"), "{}", o.section(false));
        assert!(pipes(&r).is_empty());
        assert!(!fx.marker("vm.pristine-2").exists());
    }

    #[test]
    fn a_name_taken_by_another_copy_fails_that_one_and_sends_nothing() {
        let mut fx = Fx::new();
        let other = line(101, "static/vm/vm.pristine-2", U[6], None, Some(U[4]));
        standard(
            &mut fx,
            std::slice::from_ref(&other),
            std::slice::from_ref(&other),
        );
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(has(&o, Kind::Fail, "name is taken"), "{}", o.section(false));
        assert!(pipes(&r).is_empty() && deletes(&r).is_empty());
    }

    #[test]
    fn its_own_interrupted_receive_is_deleted_at_the_start_and_only_with_its_marker() {
        let partial = line(101, "static/vm/vm.pristine-2", U[6], None, None);
        // With the marker: the one delete of a pass start, by exact path.
        let mut fx = Fx::new();
        standard(&mut fx, std::slice::from_ref(&partial), &[]);
        let m = fx.mount();
        fx.make_dir("static/vm/vm.pristine-2");
        fx.make_dir("static/vm/.incoming");
        std::fs::write(fx.marker("vm.pristine-2"), "x").unwrap();
        // After the delete the listing no longer has it.
        let after = [line(100, "vm/vm.1", U[6], None, Some(U[0]))];
        fx.list(&m, &after, &after);
        fx.say(
            &format!("btrfs subvolume delete {m}/static/vm/vm.pristine-2"),
            "",
        );
        fx.pipe(
            &format!("send -p /v/.btrbk-snapshots/vm.1 /v/{PRISTINE}"),
            (0, 0),
            None,
        );
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert_eq!(
            deletes(&r),
            [format!(
                "btrfs subvolume delete {m}/static/vm/vm.pristine-2"
            )]
        );
        assert!(
            has(&o, Kind::Pruned, "unfinished receive"),
            "{}",
            o.section(false)
        );
        assert!(!fx.marker("vm.pristine-2").exists());
        // Without the marker it is not ours: refused, never deleted.
        let mut fx = Fx::new();
        standard(&mut fx, std::slice::from_ref(&partial), &[]);
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(deletes(&r).is_empty());
        assert!(has(&o, Kind::Fail, "name is taken"), "{}", o.section(false));
    }

    #[test]
    fn a_marker_beside_a_finished_copy_or_nothing_is_removed_and_nothing_is_deleted() {
        let done = line(101, "static/vm/vm.pristine-2", U[6], None, Some(U[1]));
        let mut fx = Fx::new();
        standard(
            &mut fx,
            std::slice::from_ref(&done),
            std::slice::from_ref(&done),
        );
        fx.make_dir("static/vm/.incoming");
        std::fs::write(fx.marker("vm.pristine-2"), "x").unwrap();
        std::fs::write(fx.marker("vm.pristine-9"), "x").unwrap(); // nothing at that name
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(
            deletes(&r).is_empty() && !o.failed(),
            "{}",
            o.section(false)
        );
        assert!(!fx.marker("vm.pristine-2").exists() && !fx.marker("vm.pristine-9").exists());
        // A read-write subvolume WITH a received uuid, or read-only without
        // one, is unexplained: marker and subvolume both stay.
        for odd in [
            line(101, "static/vm/vm.pristine-2", U[6], None, Some(U[1])),
            line(101, "static/vm/vm.pristine-2", U[6], None, None),
        ] {
            let ro: Vec<String> = if odd.contains("received_uuid -") {
                vec![odd.clone()]
            } else {
                vec![]
            };
            let mut fx = Fx::new();
            standard(&mut fx, std::slice::from_ref(&odd), &ro);
            fx.make_dir("static/vm/.incoming");
            std::fs::write(fx.marker("vm.pristine-2"), "x").unwrap();
            let r = fx.runner();
            let o = fx.go(&r, false);
            assert!(deletes(&r).is_empty(), "{odd}");
            assert!(
                fx.marker("vm.pristine-2").exists(),
                "{odd}{}",
                o.section(false)
            );
        }
    }

    #[test]
    fn a_superseded_copy_is_deleted_by_exact_path_after_its_replacement_lands() {
        let mut fx = Fx::new();
        let old = line(101, "static/vm/vm.pristine-1", U[6], None, Some(U[3]));
        standard(
            &mut fx,
            std::slice::from_ref(&old),
            std::slice::from_ref(&old),
        );
        let m = fx.mount();
        fx.make_dir("static/vm/vm.pristine-1");
        fx.pipe(
            &format!("send -p /v/.btrbk-snapshots/vm.1 /v/{PRISTINE}"),
            (0, 0),
            Some("vm.pristine-2"),
        );
        fx.show("vm.pristine-2", Some(U[1]), true);
        // The listing taken after the send has the new copy.
        let new = line(102, "static/vm/vm.pristine-2", U[6], None, Some(U[1]));
        let after = [
            line(100, "vm/vm.1", U[6], None, Some(U[0])),
            old.clone(),
            new,
        ];
        fx.list(&m, &after, &after);
        fx.say(
            &format!("btrfs subvolume delete {m}/static/vm/vm.pristine-1"),
            "",
        );
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(!o.failed(), "{}", o.section(false));
        assert_eq!(
            deletes(&r),
            [format!(
                "btrfs subvolume delete {m}/static/vm/vm.pristine-1"
            )]
        );
        assert!(
            has(&o, Kind::Pruned, "vm.pristine-1"),
            "{}",
            o.section(false)
        );
        // Never on the source, never recursive, never anything else.
        assert!(
            r.calls()
                .iter()
                .all(|c| !c.contains(" -R ") || c.contains("list"))
        );
    }

    #[test]
    fn a_gone_copy_is_kept_when_its_replacement_did_not_land() {
        let mut fx = Fx::new();
        let old = line(101, "static/vm/vm.pristine-1", U[6], None, Some(U[3]));
        standard(
            &mut fx,
            std::slice::from_ref(&old),
            std::slice::from_ref(&old),
        );
        fx.make_dir("static/vm/vm.pristine-1");
        fx.pipe(
            &format!("send -p /v/.btrbk-snapshots/vm.1 /v/{PRISTINE}"),
            (0, 1),
            None,
        );
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(deletes(&r).is_empty());
        assert!(has(&o, Kind::Kept, "vm.pristine-1"), "{}", o.section(false));
        assert!(o.failed());
        assert!(fx.dir.path().join("static/vm/vm.pristine-1").exists());
    }

    #[test]
    fn a_prune_is_refused_when_the_target_no_longer_verifies_or_the_path_is_a_symlink() {
        let old = line(101, "static/vm/vm.pristine-1", U[6], None, Some(U[3]));
        let new = line(102, "static/vm/vm.pristine-2", U[6], None, Some(U[1]));
        let setup = |fx: &mut Fx| {
            standard(fx, &[old.clone(), new.clone()], &[old.clone(), new.clone()]);
            fx.say(
                &format!(
                    "btrfs subvolume delete {}/static/vm/vm.pristine-1",
                    fx.mount()
                ),
                "",
            );
        };
        // Verified at the start, not before the delete.
        let mut fx = Fx::new();
        setup(&mut fx);
        fx.make_dir("static/vm/vm.pristine-1");
        let calls = Cell::new(0);
        let r = fx.runner();
        let o = fx.go_with(&r, false, &|_, _| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Ok(())
            } else {
                Err("gone".into())
            }
        });
        assert!(deletes(&r).is_empty());
        assert!(has(&o, Kind::Fail, "gone"), "{}", o.section(false));
        // A symlink at the path is not a subvolume directory.
        let mut fx = Fx::new();
        setup(&mut fx);
        fx.make_dir("static/vm");
        fx.make_dir("elsewhere");
        std::os::unix::fs::symlink(
            fx.dir.path().join("elsewhere"),
            fx.dir.path().join("static/vm/vm.pristine-1"),
        )
        .unwrap();
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(deletes(&r).is_empty());
        assert!(has(&o, Kind::Fail, "vm.pristine-1"), "{}", o.section(false));
    }

    #[test]
    fn an_unreadable_source_or_target_or_an_unverified_target_does_nothing_and_fails() {
        // Source listing fails.
        let mut fx = Fx::new();
        let m = fx.mount();
        fx.list(&m, &[line(100, "vm/vm.1", U[6], None, Some(U[0]))], &[]);
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(has(&o, Kind::Fail, "nothing done"), "{}", o.section(false));
        assert!(pipes(&r).is_empty() && deletes(&r).is_empty());
        // Target listing fails (unscripted) while the source reads.
        let mut fx = Fx::new();
        let src = [line(20, PRISTINE, U[1], None, None)];
        fx.list("/v", &src, &src);
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(has(&o, Kind::Fail, "nothing done"), "{}", o.section(false));
        assert!(pipes(&r).is_empty() && deletes(&r).is_empty());
        // The target does not verify: not even listed.
        let mut fx = Fx::new();
        standard(&mut fx, &[], &[]);
        let r = fx.runner();
        let o = fx.go_with(&r, false, &|_, _| Err("bare mount point".into()));
        assert!(
            has(&o, Kind::Fail, "bare mount point"),
            "{}",
            o.section(false)
        );
        assert!(
            r.calls()
                .iter()
                .all(|c| !c.contains(&fx.mount()) || c.starts_with("findmnt"))
                || r.calls().iter().all(|c| !c.contains("subvolume"))
        );
        assert!(pipes(&r).is_empty() && deletes(&r).is_empty());
    }

    #[test]
    fn a_target_not_mounted_at_its_top_level_or_reached_through_a_symlink_is_refused() {
        let mut fx = Fx::new();
        standard(&mut fx, &[], &[]);
        let m = fx.mount();
        fx.answers
            .retain(|(k, ..)| *k != format!("findmnt -n -o FSROOT --target {m}"));
        fx.say(&format!("findmnt -n -o FSROOT --target {m}"), "/@sub\n");
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(has(&o, Kind::Fail, "top level"), "{}", o.section(false));
        assert!(pipes(&r).is_empty());
        let mut fx = Fx::new();
        standard(&mut fx, &[], &[]);
        fx.make_dir("elsewhere");
        std::os::unix::fs::symlink(
            fx.dir.path().join("elsewhere"),
            fx.dir.path().join("static"),
        )
        .unwrap();
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(has(&o, Kind::Fail, "symlink"), "{}", o.section(false));
        assert!(pipes(&r).is_empty() && deletes(&r).is_empty());
    }

    #[test]
    fn only_selected_sources_and_primary_targets_run_and_no_table_runs_nothing() {
        let mut fx = Fx::new();
        standard(&mut fx, &[], &[]);
        let r = fx.runner();
        let env = Env {
            runner: &r,
            is_mountpoint: &|_| true,
            verify: &|_, _| Ok(()),
        };
        // A different source selected, or only the mirror: nothing asked.
        assert!(
            run(&fx.cfg, &["other".into()], &["t".into()], false, &env)
                .tables
                .is_empty()
        );
        assert!(
            run(&fx.cfg, &["s".into()], &["m".into()], false, &env)
                .tables
                .is_empty()
        );
        assert!(r.calls().is_empty(), "{:?}", r.calls());
        // No table configured: zero commands, empty section.
        let mut none = Fx::new();
        none.cfg.sources[0].static_subvolumes.clear();
        let r = none.runner();
        let o = none.go(&r, false);
        assert!(r.calls().is_empty() && o.section(false).is_empty());
        // A source whose targets include no primary one says so.
        let mut fx = Fx::new();
        fx.cfg.sources[0].target_labels = vec!["m".into()];
        let r = fx.runner();
        let o = fx.go(&r, false);
        assert!(
            has(&o, Kind::Warn, "no primary target"),
            "{}",
            o.section(false)
        );
        assert!(!o.failed());
    }
}
