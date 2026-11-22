//! The guarded mount of a multi-device (RAID-1) backup target
//! (bd DAS-Backup-Manager-su1b.10).
//!
//! A RAID-1 target used to be mounted with `[das].mount_opts`, `degraded`
//! included, by its filesystem UUID. When the kernel had forgotten a member
//! that was physically attached, that mount came up on one leg and every job
//! said OK. Here, instead:
//!
//! 1. The members are found by filesystem UUID (`blkid -c /dev/null -t
//!    UUID=<fsid> -o device`), each registered with btrfs by its own path
//!    (`btrfs device scan <path>`; never an unscoped scan, never `--forget`),
//!    and each identified by its own superblock (`dump-super`: `fsid`,
//!    `dev_item.devid`, `dev_item.uuid`, `generation`, `num_devices`).
//! 2. With every member present the filesystem is mounted WITHOUT `degraded`.
//! 3. A member not present, or a plain mount that fails, is waited for:
//!    [`crate::config::Das::member_wait_secs`], polling every [`POLL`] and
//!    rescanning, so a merely late member never costs a degraded mount.
//! 4. Degraded only when, after the wait, a member is absent AND a configured
//!    serial is absent: every configured drive attached while a member is not
//!    found is the incident's own shape, refused. The degraded period is
//!    recorded (the present members' `dev_uuid` and superblock generation) in
//!    two copies — the primary under [`RECORD_DIR`] and a witness on the
//!    filesystem itself ([`WITNESS_NAME`]) — both written before the mount is
//!    handed to anything that writes.
//! 5. Split-brain refusal: the record names the members the last degraded
//!    period ran on; a degraded mount whose present members are not all among
//!    them is refused. The same members alone again continue the period.
//!    Every member present again: a normal mount, and the record says a scrub
//!    is owed before the pair is trusted (`btrdasd doctor` reports it; a
//!    completed scrub started after the rejoin clears it).
//! 6. Either copy unreadable or corrupt refuses a degraded mount (fail
//!    closed). Only "no such file" is "no record".
//! 7. After a normal mount the kernel's own view (`/sys/fs/btrfs/<fsid>/
//!    devinfo/<devid>/missing`) must show no member missing.
//! 8. What the job will do to the target is the caller's to say, never
//!    inferred ([`Intent`], bd DAS-Backup-Manager-su1b.11, operator
//!    2026-11-22): a job that only reads it (walk, restore, browsing, the
//!    doctor) is given the split-brain member read-only —
//!    [`STALE_READ_OPTS`], nothing replayed, no record changed — with a
//!    warning naming the date its data stops; a job that writes stays
//!    refused.
//!
//! Single-device targets never come here.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::Target;
use crate::progress::{LogLevel, ProgressCallback};

/// Where the primary copy of each RAID-1 target's record lives, one file per
/// filesystem UUID. Inside `@`: a rollback of `/` loses it, which is what the
/// witness on the filesystem itself is for.
pub const RECORD_DIR: &str = "/var/lib/das-backup/raid";
/// Overrides [`RECORD_DIR`], as `DAS_RECOVERY_OS_STATE` does its file.
pub const RECORD_DIR_ENV: &str = "DAS_RAID_RECORD_DIR";
/// The witness copy, at the top level of the target filesystem.
pub const WITNESS_NAME: &str = ".das-backup-raid.record";
/// How often a member being waited for is looked for again.
pub const POLL: Duration = Duration::from_secs(5);
/// The read-only probe mount that reads the witness before a degraded mount
/// is decided: nothing is written to the member, log replay included.
pub const PROBE_OPTS: &str = "ro,degraded,rescue=nologreplay";
/// What a read-only job's mount of a split-brain member adds to the
/// configured options (`degraded` and any `rw` taken out first).
pub const STALE_READ_OPTS: &str = "ro,degraded,rescue=nologreplay";

/// What the job that asks for a mount will do to the target. Every caller
/// says it, through [`Job::intent`]; nothing infers it from the mount
/// options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// The job writes the target, or writes state derived from its contents
    /// (the index, a record): given a split-brain member it is refused.
    Write,
    /// The job persists nothing derived from what it reads: it may read a
    /// split-brain member, read-only.
    ReadOnly,
}

/// Every job that mounts a target, and the [`Intent`] it mounts with. Only a
/// job that persists nothing derived from the target's contents (browsing,
/// restoring a file or snapshot to the host, the doctor's report) is
/// [`Intent::ReadOnly`]. A walk, an index job, reindex and reconcile
/// (`--rebuild`, `--repair` included) write the index from what the mount
/// shows, and the recovery-OS reading writes its state file: on a
/// split-brain member they would delete real index entries and record old
/// contents as current, so they are [`Intent::Write`] and refused (bd
/// DAS-Backup-Manager-su1b.11, security review 2026-11-22).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// `btrdasd backup run|snapshot|send|static`, the helper's `BackupRun`.
    Backup,
    /// `btrdasd backup boot-archive`.
    BootArchive,
    /// `btrdasd forget`, `btrdasd purge`.
    Forget,
    /// The scrub engine.
    Scrub,
    /// `btrdasd backup mount-target`, for `backup-run.sh`.
    MountTarget,
    /// `btrdasd walk`.
    Walk,
    /// The helper's `IndexWalk`.
    IndexWalk,
    /// `btrdasd reindex`.
    Reindex,
    /// `btrdasd reconcile`.
    Reconcile,
    /// `btrdasd recovery-os record` and the backup's recovery-OS reading.
    RecoveryOsRecord,
    /// `btrdasd restore file`, the helper's `RestoreFiles`.
    RestoreFiles,
    /// `btrdasd restore snapshot`, the helper's `RestoreSnapshot`.
    RestoreSnapshot,
    /// `btrdasd restore browse`.
    Browse,
    /// `btrdasd doctor`'s delivery check (a report; it records nothing).
    Doctor,
    /// `btrdasd raid accept`: it rewrites the record and the witness.
    RaidAccept,
}

impl Job {
    /// Every job, for the tests that hold the table to its rule.
    pub const ALL: [Job; 15] = [
        Job::Backup,
        Job::BootArchive,
        Job::Forget,
        Job::Scrub,
        Job::MountTarget,
        Job::Walk,
        Job::IndexWalk,
        Job::Reindex,
        Job::Reconcile,
        Job::RecoveryOsRecord,
        Job::RestoreFiles,
        Job::RestoreSnapshot,
        Job::Browse,
        Job::Doctor,
        Job::RaidAccept,
    ];

    /// The intent this job mounts with.
    pub const fn intent(self) -> Intent {
        match self {
            Job::RestoreFiles | Job::RestoreSnapshot | Job::Browse | Job::Doctor => {
                Intent::ReadOnly
            }
            Job::Backup
            | Job::BootArchive
            | Job::Forget
            | Job::Scrub
            | Job::MountTarget
            | Job::Walk
            | Job::IndexWalk
            | Job::Reindex
            | Job::Reconcile
            | Job::RecoveryOsRecord
            | Job::RaidAccept => Intent::Write,
        }
    }
}
/// The first line of every record copy.
const FORMAT_LINE: &str = "format=das-backup-raid-record 1";

/// The record directory in force: [`RECORD_DIR_ENV`], else [`RECORD_DIR`].
pub fn record_dir() -> PathBuf {
    std::env::var_os(RECORD_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(RECORD_DIR))
}

/// The primary copy of `fsid`'s record.
pub fn primary_path(dir: &Path, fsid: &str) -> PathBuf {
    dir.join(format!("{}.record", fsid.to_ascii_lowercase()))
}

/// The witness copy on the filesystem mounted at `mount`.
pub fn witness_path(mount: &str) -> PathBuf {
    Path::new(mount).join(WITNESS_NAME)
}

/// Whether `target` is mounted through this guard: a filesystem UUID and more
/// than one configured serial. The same test as `backup-run.sh`'s
/// `is_guarded_target`.
pub fn is_guarded(target: &Target) -> bool {
    target.mount_uuid.as_deref().is_some_and(|u| !u.is_empty())
        && target.effective_serials().len() > 1
}

// ---------------------------------------------------------------------------
// Mount options
// ---------------------------------------------------------------------------

fn tokens(opts: &str) -> impl Iterator<Item = &str> {
    opts.split(',').map(str::trim).filter(|o| !o.is_empty())
}

/// `opts` with every exact `degraded` token removed.
pub fn without_degraded(opts: &str) -> String {
    tokens(opts)
        .filter(|o| *o != "degraded")
        .collect::<Vec<_>>()
        .join(",")
}

/// `opts` with exactly one `degraded`, last.
pub fn with_degraded(opts: &str) -> String {
    let base = without_degraded(opts);
    if base.is_empty() {
        "degraded".into()
    } else {
        format!("{base},degraded")
    }
}

/// `opts` for a read-only job's mount of a split-brain member: the configured
/// options without `degraded`, `rw` or `ro`, then [`STALE_READ_OPTS`].
pub fn stale_read_opts(opts: &str) -> String {
    let base: Vec<&str> = tokens(opts)
        .filter(|o| !matches!(*o, "degraded" | "rw" | "ro"))
        .collect();
    if base.is_empty() {
        STALE_READ_OPTS.into()
    } else {
        format!("{},{STALE_READ_OPTS}", base.join(","))
    }
}

/// `epoch` as the host's local date and time, or the bare number when the C
/// library cannot convert it.
pub fn when(epoch: i64) -> String {
    crate::caldate::local_datetime(epoch).unwrap_or_else(|| format!("epoch {epoch}"))
}

/// Whether `opts` asks for a read-only mount.
pub fn is_read_only(opts: &str) -> bool {
    tokens(opts).any(|o| o == "ro")
}

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

/// One member of the filesystem, as its own superblock describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub path: String,
    pub devid: u64,
    pub dev_uuid: String,
    pub generation: u64,
    pub num_devices: u64,
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Parse `btrfs inspect-internal dump-super <path>` output. Each key is the
/// line's first token, matched exactly (`generation`, never
/// `chunk_root_generation`; `fsid`, never `dev_item.fsid`), and must appear
/// exactly once; `fsid` must be `fsid`. Anything else is an error, never a
/// member.
pub fn parse_super(path: &str, text: &str, fsid: &str) -> Result<Member, String> {
    let mut found: [Option<&str>; 5] = [None; 5];
    const KEYS: [&str; 5] = [
        "fsid",
        "dev_item.devid",
        "dev_item.uuid",
        "generation",
        "num_devices",
    ];
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let Some(i) = KEYS.iter().position(|k| *k == key) {
            if found[i].is_some() {
                return Err(format!("{path}: superblock lists '{key}' twice"));
            }
            found[i] = Some(value);
        }
    }
    let get =
        |i: usize| found[i].ok_or_else(|| format!("{path}: superblock has no '{}' line", KEYS[i]));
    let number = |i: usize| -> Result<u64, String> {
        let v = get(i)?;
        v.parse::<u64>()
            .map_err(|_| format!("{path}: superblock '{}' is '{v}', not a number", KEYS[i]))
    };
    let sb_fsid = get(0)?;
    if !sb_fsid.eq_ignore_ascii_case(fsid) {
        return Err(format!(
            "{path}: superblock fsid is {sb_fsid}, not {fsid} — not a member of this filesystem"
        ));
    }
    let dev_uuid = get(2)?;
    if !is_uuid(dev_uuid) {
        return Err(format!(
            "{path}: superblock dev_item.uuid '{dev_uuid}' is not a UUID"
        ));
    }
    let devid = number(1)?;
    let num_devices = number(4)?;
    if devid == 0 || num_devices == 0 {
        return Err(format!(
            "{path}: superblock devid {devid} / num_devices {num_devices} cannot be a member"
        ));
    }
    Ok(Member {
        path: path.to_string(),
        devid,
        dev_uuid: dev_uuid.to_ascii_lowercase(),
        generation: number(3)?,
        num_devices,
    })
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// Where a target's degraded history stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The filesystem is (or was last) mounted with a member absent.
    Degraded,
    /// Every member is present again; a scrub is owed before the pair is
    /// trusted.
    ScrubOwed,
    /// No degraded period is open and no scrub is owed, but members retired
    /// by `btrdasd raid accept` ([`Record::rejected`]) are still refused. Only
    /// with at least one rejected member.
    Settled,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Degraded => "degraded",
            Self::ScrubOwed => "scrub-owed",
            Self::Settled => "settled",
        }
    }

    /// How cautious the state is, for merging two copies: the more cautious
    /// wins.
    fn caution(self) -> u8 {
        match self {
            Self::Degraded => 2,
            Self::ScrubOwed => 1,
            Self::Settled => 0,
        }
    }
}

/// One member the degraded period ran on: its devid, its per-device UUID
/// (`dev_item.uuid`; a `btrfs replace` reuses devids, never device UUIDs)
/// and its superblock generation when the period first saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodMember {
    pub devid: u64,
    pub dev_uuid: String,
    pub generation: u64,
}

/// A target's record of its last degraded period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub fsid: String,
    pub state: State,
    /// The members the period ran on (never empty).
    pub members: Vec<PeriodMember>,
    /// When the period began, and when a degraded mount last continued it
    /// (epoch seconds).
    pub since: i64,
    pub last: i64,
    /// When every member was first seen present again (scrub-owed only).
    pub rejoined: Option<i64>,
    /// Members `btrdasd raid accept` gave up (bd DAS-Backup-Manager-su1b.11):
    /// refused, alone or beside the others, while their superblock is found.
    /// Each is `<devid> <dev_uuid> <generation>`, its generation when the
    /// period it ran on was recorded. Kept across the scrub (state
    /// [`State::Settled`]).
    pub rejected: Vec<PeriodMember>,
}

impl Record {
    /// The per-device UUIDs the period ran on.
    pub fn dev_uuids(&self) -> BTreeSet<&str> {
        self.members.iter().map(|m| m.dev_uuid.as_str()).collect()
    }

    /// The per-device UUIDs `btrdasd raid accept` gave up.
    pub fn rejected_uuids(&self) -> BTreeSet<&str> {
        self.rejected.iter().map(|m| m.dev_uuid.as_str()).collect()
    }

    /// The text of one copy.
    pub fn render(&self) -> String {
        let mut out = format!(
            "{FORMAT_LINE}\nfsid={}\nstate={}\n",
            self.fsid,
            self.state.as_str()
        );
        for m in &self.members {
            out.push_str(&format!(
                "member={} {} {}\n",
                m.devid, m.dev_uuid, m.generation
            ));
        }
        for m in &self.rejected {
            out.push_str(&format!(
                "rejected={} {} {}\n",
                m.devid, m.dev_uuid, m.generation
            ));
        }
        out.push_str(&format!("since={}\nlast={}\n", self.since, self.last));
        if let Some(r) = self.rejoined {
            out.push_str(&format!("rejoined={r}\n"));
        }
        out
    }

    /// Parse one copy, strictly: the format line first, every key known,
    /// each once (`member` one or more times, `rejected` any number),
    /// `rejoined` exactly when the state is `scrub-owed`, `settled` only with
    /// a `rejected` line, and `fsid` equal to `fsid`.
    pub fn parse(text: &str, fsid: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        if lines.next() != Some(FORMAT_LINE) {
            return Err(format!("the first line is not '{FORMAT_LINE}'"));
        }
        let (mut rec_fsid, mut state, mut since, mut last, mut rejoined) =
            (None, None, None, None, None);
        let mut members = Vec::new();
        let mut rejected = Vec::new();
        for line in lines {
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("'{line}' is not key=value"));
            };
            let once = |slot: &mut Option<String>| {
                if slot.is_some() {
                    Err(format!("'{key}' appears twice"))
                } else {
                    *slot = Some(value.to_string());
                    Ok(())
                }
            };
            match key {
                "fsid" => once(&mut rec_fsid)?,
                "state" => once(&mut state)?,
                "since" => once(&mut since)?,
                "last" => once(&mut last)?,
                "rejoined" => once(&mut rejoined)?,
                "member" => members.push(parse_member(key, value)?),
                "rejected" => {
                    let m = parse_member(key, value)?;
                    if rejected
                        .iter()
                        .any(|r: &PeriodMember| r.dev_uuid == m.dev_uuid)
                    {
                        return Err(format!("rejected '{value}' appears twice"));
                    }
                    rejected.push(m);
                }
                _ => return Err(format!("unknown key '{key}'")),
            }
        }
        let need = |v: Option<String>, k: &str| v.ok_or_else(|| format!("no '{k}' line"));
        let rec_fsid = need(rec_fsid, "fsid")?;
        if !rec_fsid.eq_ignore_ascii_case(fsid) {
            return Err(format!("it is the record of {rec_fsid}, not {fsid}"));
        }
        let state = match need(state, "state")?.as_str() {
            "degraded" => State::Degraded,
            "scrub-owed" => State::ScrubOwed,
            "settled" => State::Settled,
            other => {
                return Err(format!(
                    "state '{other}' is not degraded, scrub-owed or settled"
                ));
            }
        };
        let epoch = |v: String, k: &str| {
            v.parse::<i64>()
                .map_err(|_| format!("'{k}' is '{v}', not a time"))
        };
        let since = epoch(need(since, "since")?, "since")?;
        let last = epoch(need(last, "last")?, "last")?;
        let rejoined = rejoined.map(|v| epoch(v, "rejoined")).transpose()?;
        match (state, rejoined) {
            (State::ScrubOwed, None) => return Err("scrub-owed without 'rejoined'".into()),
            (State::Degraded, Some(_)) => return Err("degraded with 'rejoined'".into()),
            (State::Settled, Some(_)) => return Err("settled with 'rejoined'".into()),
            _ => {}
        }
        if members.is_empty() {
            return Err("no 'member' line".into());
        }
        if state == State::Settled && rejected.is_empty() {
            return Err("settled without a 'rejected' line".into());
        }
        if let Some(m) = members
            .iter()
            .find(|m| rejected.iter().any(|r| r.dev_uuid == m.dev_uuid))
        {
            return Err(format!(
                "device {} is both a member and rejected",
                m.dev_uuid
            ));
        }
        Ok(Self {
            fsid: rec_fsid.to_ascii_lowercase(),
            state,
            members,
            since,
            last,
            rejoined,
            rejected,
        })
    }
}

/// One `member=` or `rejected=` value: `<devid> <dev_uuid> <generation>`.
fn parse_member(key: &str, value: &str) -> Result<PeriodMember, String> {
    let parts: Vec<&str> = value.split(' ').collect();
    let [devid, dev_uuid, generation] = parts[..] else {
        return Err(format!(
            "{key} '{value}' is not '<devid> <dev_uuid> <generation>'"
        ));
    };
    let (Ok(devid), Ok(generation)) = (devid.parse(), generation.parse()) else {
        return Err(format!(
            "{key} '{value}' has a devid or generation that is not a number"
        ));
    };
    if !is_uuid(dev_uuid) {
        return Err(format!("{key} '{value}' has no device UUID"));
    }
    Ok(PeriodMember {
        devid,
        dev_uuid: dev_uuid.to_ascii_lowercase(),
        generation,
    })
}

/// What reading one copy found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Copy {
    /// No such file: no degraded period recorded in this copy.
    Absent,
    Read(Record),
    /// Unreadable, not a regular file, or not a well-formed record of this
    /// filesystem. Never "no record".
    Bad(String),
}

/// Read one copy. Only `ENOENT` is [`Copy::Absent`].
pub fn read_copy(path: &Path, fsid: &str) -> Copy {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Copy::Absent,
        Err(e) => return Copy::Bad(format!("{}: {e}", path.display())),
        Ok(m) if !m.file_type().is_file() => {
            return Copy::Bad(format!("{}: not a regular file", path.display()));
        }
        Ok(_) => {}
    }
    match fs::read_to_string(path) {
        Ok(text) => match Record::parse(&text, fsid) {
            Ok(r) => Copy::Read(r),
            Err(e) => Copy::Bad(format!("{}: {e}", path.display())),
        },
        Err(e) => Copy::Bad(format!("{}: {e}", path.display())),
    }
}

/// Write one copy whole: a temporary file beside it, synced, renamed over it,
/// the directory synced, then read back and compared. `make_dir` creates a
/// missing parent (0700) — for the primary copy only; the witness's parent is
/// the mounted filesystem's top level.
pub fn write_copy(path: &Path, record: &Record, make_dir: bool) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    if make_dir {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let text = record.render();
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("record"),
        std::process::id()
    ));
    let written = (|| -> io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(dir)?.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", path.display()));
    }
    match read_copy(path, &record.fsid) {
        Copy::Read(back) if &back == record => Ok(()),
        Copy::Read(_) => Err(format!(
            "{}: read back, it is not what was written",
            path.display()
        )),
        Copy::Absent => Err(format!("{}: gone straight after writing", path.display())),
        Copy::Bad(e) => Err(format!("read back: {e}")),
    }
}

/// Remove one copy; an absent one is already removed.
pub fn remove_copy(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
    }
}

/// The record the two copies hold together. Either unreadable, or the two
/// naming different members, is an error; one copy absent is the other.
pub fn merged(primary: &Copy, witness: &Copy) -> Result<Option<Record>, String> {
    match (primary, witness) {
        (Copy::Bad(e), _) => Err(format!("the record is unreadable ({e})")),
        (_, Copy::Bad(e)) => Err(format!("the witness record is unreadable ({e})")),
        (Copy::Absent, Copy::Absent) => Ok(None),
        (Copy::Read(r), Copy::Absent) | (Copy::Absent, Copy::Read(r)) => Ok(Some(r.clone())),
        (Copy::Read(a), Copy::Read(b)) => {
            if a.dev_uuids() != b.dev_uuids() {
                return Err(format!(
                    "the record names members {} and the witness {} — they disagree",
                    names(&a.dev_uuids()),
                    names(&b.dev_uuids())
                ));
            }
            let state = if a.state.caution() >= b.state.caution() {
                a.state
            } else {
                b.state
            };
            // Every member either copy rejected stays rejected.
            let mut rejected = a.rejected.clone();
            for r in &b.rejected {
                if !rejected.iter().any(|o| o.dev_uuid == r.dev_uuid) {
                    rejected.push(r.clone());
                }
            }
            Ok(Some(Record {
                fsid: a.fsid.clone(),
                state,
                members: a.members.clone(),
                since: a.since.min(b.since),
                last: a.last.max(b.last),
                rejoined: if state == State::ScrubOwed {
                    a.rejoined.max(b.rejoined)
                } else {
                    None
                },
                rejected,
            }))
        }
    }
}

fn names(set: &BTreeSet<&str>) -> String {
    set.iter().copied().collect::<Vec<_>>().join(", ")
}

/// Whether a degraded mount on `present` may go ahead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Yes; this is the record to write before it is used.
    Allow(Record),
    /// No: the present members lack the writes of the last degraded period,
    /// which ran on others (`record`). Refused for writing, whatever the
    /// state; a read-only job may read them ([`Intent::ReadOnly`]).
    SplitBrain { record: Record, why: String },
    /// No, for any job: a copy unreadable, or the copies disagree.
    Refuse(String),
}

/// The split-brain rule. No record: a new period on `present`. A record: the
/// present members must all be ones the last degraded period ran on, or they
/// lack that period's writes — refused, whatever the state (a scrub owed
/// means the pair is not yet trusted). Either copy unreadable: refused.
pub fn degraded_verdict(
    fsid: &str,
    primary: &Copy,
    witness: &Copy,
    present: &[Member],
    now: i64,
) -> Verdict {
    let record = match merged(primary, witness) {
        Ok(r) => r,
        Err(e) => {
            return Verdict::Refuse(format!(
                "{e} — a degraded mount is refused while the record of the last degraded \
                 period cannot be read"
            ));
        }
    };
    let Some(record) = record else {
        return Verdict::Allow(Record {
            fsid: fsid.to_ascii_lowercase(),
            state: State::Degraded,
            members: present
                .iter()
                .map(|m| PeriodMember {
                    devid: m.devid,
                    dev_uuid: m.dev_uuid.clone(),
                    generation: m.generation,
                })
                .collect(),
            since: now,
            last: now,
            rejoined: None,
            rejected: Vec::new(),
        });
    };
    if let Some(why) = rejected_present(&record, present) {
        return Verdict::Refuse(why);
    }
    if record.state == State::Settled {
        // No period open: a new one on `present`, the rejection kept.
        return Verdict::Allow(Record {
            state: State::Degraded,
            members: present
                .iter()
                .map(|m| PeriodMember {
                    devid: m.devid,
                    dev_uuid: m.dev_uuid.clone(),
                    generation: m.generation,
                })
                .collect(),
            since: now,
            last: now,
            rejoined: None,
            ..record
        });
    }
    let period = record.dev_uuids();
    let strangers: Vec<String> = present
        .iter()
        .filter(|m| !period.contains(m.dev_uuid.as_str()))
        .map(|m| format!("devid {} ({})", m.devid, m.dev_uuid))
        .collect();
    if !strangers.is_empty() {
        let why = format!(
            "split brain: the last degraded period (since {}) ran on {}, and {} — which \
             lacks that period's writes — is the only member present{}; mounting it degraded \
             would give the filesystem two diverging histories",
            record.since,
            names(&period),
            strangers.join(", "),
            if record.state == State::ScrubOwed {
                " (the scrub owed since the rejoin has not completed)"
            } else {
                ""
            }
        );
        return Verdict::SplitBrain { record, why };
    }
    let members = record
        .members
        .iter()
        .filter(|m| present.iter().any(|p| p.dev_uuid == m.dev_uuid))
        .cloned()
        .collect();
    Verdict::Allow(Record {
        state: State::Degraded,
        members,
        last: now,
        rejoined: None,
        ..record
    })
}

/// The refusal of a present member `btrdasd raid accept` gave up, if one is
/// among `present`.
pub fn rejected_present(record: &Record, present: &[Member]) -> Option<String> {
    let rejected = record.rejected_uuids();
    let found: Vec<&Member> = present
        .iter()
        .filter(|m| rejected.contains(m.dev_uuid.as_str()))
        .collect();
    (!found.is_empty()).then(|| rejected_why(&found))
}

fn rejected_why(found: &[&Member]) -> String {
    format!(
        "{} was given up by 'btrdasd raid accept' and lacks the writes made since: \
         refused, alone or beside the other members, until it is wiped (wipefs) or \
         removed from the filesystem (btrfs device remove, btrfs replace) — take it out \
         of the enclosure",
        found
            .iter()
            .map(|m| format!("devid {} ({}) at {}", m.devid, m.dev_uuid, m.path))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// What a normal mount (every member present) does with the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejoin {
    /// No degraded period recorded.
    Clean,
    /// A scrub is owed: `record` is what both copies must say.
    Owed {
        record: Record,
        write_primary: bool,
        write_witness: bool,
        /// The record was degraded: this mount is the rejoin.
        rejoined_now: bool,
    },
    /// A copy could not be read: nothing is changed.
    Unreadable(String),
}

/// The rejoin rule: a degraded record becomes scrub-owed, rejoined now.
pub fn rejoin(primary: &Copy, witness: &Copy, now: i64) -> Rejoin {
    let record = match merged(primary, witness) {
        Ok(None) => return Rejoin::Clean,
        Ok(Some(r)) => r,
        Err(e) => return Rejoin::Unreadable(e),
    };
    let rejoined_now = record.state == State::Degraded;
    let record = match record.state {
        State::Degraded => Record {
            state: State::ScrubOwed,
            rejoined: Some(now),
            ..record
        },
        State::ScrubOwed => record,
        State::Settled => return Rejoin::Clean,
    };
    Rejoin::Owed {
        rejoined_now,
        write_primary: *primary != Copy::Read(record.clone()),
        write_witness: *witness != Copy::Read(record.clone()),
        record,
    }
}

/// Whether a scrub that began at `scrub_started` and completed with nothing
/// uncorrectable pays the debt in `record`: only a scrub begun after every
/// member was present again.
pub fn scrub_clears(record: &Record, scrub_started: i64) -> bool {
    record.state == State::ScrubOwed && record.rejoined.is_some_and(|r| scrub_started > r)
}

/// After a completed scrub (finished, nothing uncorrectable) of the
/// filesystem mounted at `mount`, begun at `scrub_started`: remove both copies
/// of its record when that scrub pays the debt ([`scrub_clears`]) and the
/// kernel shows every member present. The witness goes first: a copy left
/// behind by a failed removal re-opens a paid debt, the safe direction.
/// `Ok(Some)` is the record cleared; `Ok(None)` nothing to clear (no record,
/// still degraded, or a scrub begun before the rejoin); `Err` a copy that
/// cannot be read, a member missing, or a removal that failed — nothing is
/// cleared on doubt.
pub fn clear_after_scrub(
    record_dir: &Path,
    mount: &str,
    fsid: &str,
    scrub_started: i64,
    missing: Result<Vec<u64>, String>,
) -> Result<Option<Record>, String> {
    let primary_path = primary_path(record_dir, fsid);
    let witness_path = witness_path(mount);
    let primary = read_copy(&primary_path, fsid);
    let witness = read_copy(&witness_path, fsid);
    let Some(record) = merged(&primary, &witness)? else {
        return Ok(None);
    };
    if !scrub_clears(&record, scrub_started) {
        return Ok(None);
    }
    match missing {
        Ok(m) if m.is_empty() => {}
        Ok(m) => return Err(format!("the kernel shows devid(s) {m:?} missing")),
        Err(e) => return Err(format!("whether a member is missing cannot be read ({e})")),
    }
    if record.rejected.is_empty() {
        remove_copy(&witness_path)?;
        remove_copy(&primary_path)?;
    } else {
        // The debt is paid, the rejection is not lifted: `settled`, witness
        // first (a primary left `scrub-owed` merges as the more cautious).
        let settled = Record {
            state: State::Settled,
            rejoined: None,
            ..record.clone()
        };
        write_copy(&witness_path, &settled, false)?;
        write_copy(&primary_path, &settled, true)?;
    }
    Ok(Some(record))
}

// ---------------------------------------------------------------------------
// The kernel's own view
// ---------------------------------------------------------------------------

/// The devids the kernel marks missing for a mounted filesystem:
/// `<root>/<fsid>/devinfo/<devid>/missing` reading `1`. An unreadable
/// directory or flag is an error, never "none missing".
pub fn missing_devids_under(root: &Path, fsid: &str) -> Result<Vec<u64>, String> {
    let dir = root.join(fsid.to_ascii_lowercase()).join("devinfo");
    let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut missing = Vec::new();
    let mut seen = 0;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name();
        let Some(devid) = name.to_str().and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        seen += 1;
        let flag = entry.path().join("missing");
        let text = fs::read_to_string(&flag).map_err(|e| format!("{}: {e}", flag.display()))?;
        match text.trim() {
            "0" => {}
            "1" => missing.push(devid),
            other => {
                return Err(format!("{}: '{other}', neither 0 nor 1", flag.display()));
            }
        }
    }
    if seen == 0 {
        return Err(format!("{}: lists no device", dir.display()));
    }
    missing.sort_unstable();
    Ok(missing)
}

/// Every devid the kernel lists for a mounted filesystem, and whether it is
/// missing: `<root>/<fsid>/devinfo/<devid>/missing`. An unreadable directory
/// or flag is an error.
pub fn devinfo_under(root: &Path, fsid: &str) -> Result<Vec<(u64, bool)>, String> {
    let dir = root.join(fsid.to_ascii_lowercase()).join("devinfo");
    let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name();
        let Some(devid) = name.to_str().and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        let flag = entry.path().join("missing");
        let text = fs::read_to_string(&flag).map_err(|e| format!("{}: {e}", flag.display()))?;
        match text.trim() {
            "0" => out.push((devid, false)),
            "1" => out.push((devid, true)),
            other => {
                return Err(format!("{}: '{other}', neither 0 nor 1", flag.display()));
            }
        }
    }
    if out.is_empty() {
        return Err(format!("{}: lists no device", dir.display()));
    }
    out.sort_unstable();
    Ok(out)
}

/// [`missing_devids_under`] `/sys/fs/btrfs`.
pub fn missing_devids(fsid: &str) -> Result<Vec<u64>, String> {
    missing_devids_under(Path::new("/sys/fs/btrfs"), fsid)
}

// ---------------------------------------------------------------------------
// The guarded mount
// ---------------------------------------------------------------------------

/// What the guarded mount asks of the host. Production: `mount.rs`'s runner
/// and probes; tests: a described host. Nothing here enumerates devices: only
/// the filesystem's own UUID and the configured serials are looked up.
pub trait MemberHost {
    /// Partitions holding filesystem `fsid` (`blkid -c /dev/null -t
    /// UUID=<fsid> -o device`); none found is `Ok(vec![])`.
    fn members(&self, fsid: &str) -> Result<Vec<String>, String>;
    /// `btrfs device scan <path>`: register this one device.
    fn scan(&self, path: &str) -> Result<(), String>;
    /// `btrfs device scan --forget <path>`: unregister this one device, by
    /// its exact path (never unscoped).
    fn forget(&self, path: &str) -> Result<(), String>;
    /// `mount -t btrfs -o <opts> <device> <at>`: one member by its own path.
    fn mount_device(&self, opts: &str, device: &str, at: &str) -> Result<(), String>;
    /// A new private, empty directory to probe-mount one member at.
    fn temp_mountpoint(&self) -> Result<String, String>;
    /// Remove the directory [`MemberHost::temp_mountpoint`] made.
    fn remove_mountpoint(&self, at: &str) -> Result<(), String>;
    /// Whether filesystem `fsid` is mounted anywhere now.
    fn fsid_mounted(&self, fsid: &str) -> Result<bool, String>;
    /// `btrfs inspect-internal dump-super <path>`.
    fn read_super(&self, path: &str) -> Result<String, String>;
    /// Whether a drive with this serial is attached.
    fn serial_attached(&self, serial: &str) -> bool;
    /// `mount -t btrfs [-o opts] UUID=<fsid> <mount>`.
    fn mount(&self, opts: &str, fsid: &str, mount: &str) -> Result<(), String>;
    fn umount(&self, mount: &str) -> Result<(), String>;
    /// The kernel's missing devids for the mounted filesystem.
    fn missing_devids(&self, fsid: &str) -> Result<Vec<u64>, String>;
    /// Every devid the kernel lists for the mounted filesystem, and whether
    /// it is missing ([`devinfo_under`]).
    fn devinfo(&self, fsid: &str) -> Result<Vec<(u64, bool)>, String>;
    /// The witness copy on the filesystem now mounted at `mount`.
    fn read_witness(&self, mount: &str, fsid: &str) -> Copy;
    fn write_witness(&self, mount: &str, record: &Record) -> Result<(), String>;
    fn pause(&self, how_long: Duration);
    /// Epoch seconds.
    fn now(&self) -> i64;
}

/// One guarded mount: which filesystem, where, and with what.
pub struct Plan<'a> {
    pub label: &'a str,
    pub fsid: &'a str,
    pub mount: &'a str,
    pub serials: &'a [String],
    pub mount_opts: &'a str,
    pub wait: Duration,
    pub record_dir: &'a Path,
    /// What the job will do to the target; never inferred.
    pub intent: Intent,
}

/// How a guarded mount came up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every member present, mounted without `degraded`.
    Normal {
        members: Vec<Member>,
        /// How long a late member was waited for.
        waited: Duration,
        /// A degraded period is behind it: a scrub is owed.
        owed: Option<Record>,
        /// A record copy that could not be read or written (WARN; nothing
        /// changed).
        record_problem: Option<String>,
        /// This mount turned a degraded record into scrub-owed (the rejoin):
        /// the job that made it starts the owed scrub once it has let its
        /// locks go (bd DAS-Backup-Manager-su1b.14).
        rejoined_now: bool,
    },
    /// Split brain, for a read-only job: mounted read-only
    /// ([`STALE_READ_OPTS`]) on `members`, which lack the writes of the
    /// degraded period in `record` (unchanged). Nothing is written to it.
    StaleReadOnly {
        members: Vec<Member>,
        absent_serials: Vec<String>,
        expected: u64,
        record: Record,
    },
    /// No readable record can vouch for the members (the primary copy is
    /// unreadable, or absent while their superblock generations differ), for
    /// a read-only job: mounted read-only ([`STALE_READ_OPTS`]), nothing
    /// written. A job that writes is refused.
    ReadOnly { members: Vec<Member>, why: String },
    /// A member is absent: mounted degraded on `members`.
    Degraded {
        members: Vec<Member>,
        absent_serials: Vec<String>,
        expected: u64,
        record: Record,
        /// `false` only on a read-only mount, which writes nothing.
        recorded: bool,
    },
}

impl Outcome {
    /// The one-line status this mount reports: `OK`, `SCRUB-OWED`,
    /// `DEGRADED` or `STALE-READ-ONLY`, and the detail.
    pub fn status_line(&self) -> (&'static str, String) {
        match self {
            Self::Normal {
                members,
                waited,
                owed,
                record_problem,
                rejoined_now,
            } => {
                let base = format!(
                    "all {} members present ({}){}",
                    members.len(),
                    member_list(members),
                    if waited.is_zero() {
                        String::new()
                    } else {
                        format!(", after waiting {}s for a late member", waited.as_secs())
                    }
                );
                if let Some(problem) = record_problem {
                    ("WARN", format!("{base}; RAID record problem: {problem}"))
                } else if let Some(r) = owed {
                    (
                        if *rejoined_now {
                            "REJOINED"
                        } else {
                            "SCRUB-OWED"
                        },
                        format!(
                            "{base}; a scrub is owed before the pair is trusted: degraded on {} \
                             from {} to {}, rejoined {}{}",
                            names(&r.dev_uuids()),
                            r.since,
                            r.last,
                            r.rejoined.unwrap_or(r.last),
                            if *rejoined_now {
                                "; the scrub starts once this job has ended"
                            } else {
                                ""
                            }
                        ),
                    )
                } else {
                    ("OK", base)
                }
            }
            Self::ReadOnly { members, why } => (
                "READ-ONLY",
                format!(
                    "mounted READ-ONLY ({STALE_READ_OPTS}) for a job that only reads, nothing \
                     written: {why}; members {}",
                    member_list(members)
                ),
            ),
            Self::StaleReadOnly {
                members,
                absent_serials,
                expected,
                record,
            } => (
                "STALE-READ-ONLY",
                format!(
                    "split brain, mounted READ-ONLY ({STALE_READ_OPTS}) for a job that only \
                     reads: {} of {expected} members present ({}), which lack the writes of the \
                     last degraded period — it ran on {} from {} to {}{} — so the data here \
                     stops no later than {}; absent drive(s) {}; nothing is written to it and \
                     the record is unchanged; every job that writes the target stays refused \
                     until that drive returns (or 'btrdasd raid accept')",
                    members.len(),
                    members
                        .iter()
                        .map(|m| format!(
                            "devid {} {} {}, superblock generation {}",
                            m.devid, m.dev_uuid, m.path, m.generation
                        ))
                        .collect::<Vec<_>>()
                        .join("; "),
                    names(&record.dev_uuids()),
                    when(record.since),
                    when(record.last),
                    if record.state == State::ScrubOwed {
                        ", rejoined and not yet scrubbed"
                    } else {
                        ""
                    },
                    when(record.since),
                    if absent_serials.is_empty() {
                        "unknown (no serial configured)".to_string()
                    } else {
                        absent_serials.join(", ")
                    },
                ),
            ),
            Self::Degraded {
                members,
                absent_serials,
                expected,
                record,
                recorded,
            } => (
                "DEGRADED",
                format!(
                    "RAID-1 MOUNTED DEGRADED: {} of {expected} members present ({}); absent \
                     drive(s) {}; every write has one copy until the pair is rebuilt and \
                     scrubbed; degraded since {}{}",
                    members.len(),
                    member_list(members),
                    if absent_serials.is_empty() {
                        "unknown (no serial configured)".to_string()
                    } else {
                        absent_serials.join(", ")
                    },
                    record.since,
                    if *recorded {
                        ""
                    } else {
                        " (read-only mount: the record is not updated)"
                    }
                ),
            ),
        }
    }
}

fn member_list(members: &[Member]) -> String {
    members
        .iter()
        .map(|m| format!("devid {} {} {}", m.devid, m.dev_uuid, m.path))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Why a guarded mount did not come up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// No member of the filesystem is present: the target is not available.
    NoMember(String),
    /// A mount failed: the target is not available.
    MountFailed(String),
    /// Refused: a split brain, an unreadable record, the kernel missing a
    /// member while every drive is attached, or a record that could not be
    /// written. `left_mounted` when the refusal could not take its own mount
    /// back down.
    Refused { why: String, left_mounted: bool },
}

impl Failure {
    pub fn why(&self) -> &str {
        match self {
            Self::NoMember(w) | Self::MountFailed(w) | Self::Refused { why: w, .. } => w,
        }
    }
}

/// The one line `btrdasd backup mount-target` prints for `backup-run.sh`, and
/// its exit status: `RAID GUARD: <STATUS>  (<detail>)`, two spaces, with
/// STATUS one of `OK`, `WARN`, `SCRUB-OWED`, `REJOINED` (a scrub is owed and
/// this mount is the rejoin: the caller starts it once it has let its locks
/// go), `DEGRADED`, `STALE-READ-ONLY` (exit 0: mounted),
/// `NOT-MOUNTED` (exit 1) or `REFUSED` (exit 3; `left_mounted` adds
/// `still mounted` to the detail).
pub fn cli_line(result: &Result<Outcome, Failure>) -> (String, i32) {
    let (status, detail, code) = match result {
        Ok(outcome) => {
            let (status, detail) = outcome.status_line();
            (status, detail, 0)
        }
        Err(Failure::Refused { why, left_mounted }) => (
            "REFUSED",
            if *left_mounted {
                format!("{why} (still mounted)")
            } else {
                why.clone()
            },
            3,
        ),
        Err(f @ (Failure::NoMember(_) | Failure::MountFailed(_))) => {
            ("NOT-MOUNTED", f.why().to_string(), 1)
        }
    };
    (
        format!("RAID GUARD: {status}  ({})", detail.replace('\n', " ")),
        code,
    )
}

fn refused(why: String) -> Failure {
    Failure::Refused {
        why,
        left_mounted: false,
    }
}

/// What one look at the members found.
struct Survey {
    members: Vec<Member>,
    expected: u64,
    /// Members the record's rejection names: never scanned, never counted.
    rejected: Vec<Member>,
}

impl Survey {
    fn all_present(&self) -> bool {
        !self.members.is_empty() && self.members.len() as u64 >= self.expected
    }
}

/// One look at the members. Each superblock is read before its device is
/// registered with btrfs: a member in `rejected` (dev_uuids) is never
/// scanned, so the kernel never learns of it.
fn survey(
    host: &dyn MemberHost,
    plan: &Plan<'_>,
    rejected: &BTreeSet<String>,
    progress: &dyn ProgressCallback,
) -> Survey {
    let warn =
        |m: String| progress.on_log(LogLevel::Warning, &format!("Target '{}': {m}", plan.label));
    let paths = host.members(plan.fsid).unwrap_or_else(|e| {
        warn(format!("cannot list the members of {}: {e}", plan.fsid));
        Vec::new()
    });
    let mut members: Vec<Member> = Vec::new();
    let mut refused: Vec<Member> = Vec::new();
    for path in paths {
        let member = host
            .read_super(&path)
            .and_then(|text| parse_super(&path, &text, plan.fsid));
        let m = match member {
            Ok(m) if rejected.contains(&m.dev_uuid) => {
                refused.push(m);
                continue;
            }
            Ok(m) => m,
            Err(e) => {
                // Registered all the same, as before: a superblock this
                // parser cannot read is not a reason to hide a member.
                if let Err(e) = host.scan(&path) {
                    warn(format!("btrfs device scan {path} failed: {e}"));
                }
                warn(format!("not counted as a member: {e}"));
                continue;
            }
        };
        if let Err(e) = host.scan(&path) {
            warn(format!("btrfs device scan {path} failed: {e}"));
        }
        if members.iter().any(|o| o.devid == m.devid) {
            warn(format!("{path} is devid {} again — counted once", m.devid));
        } else {
            members.push(m);
        }
    }
    let expected = members.iter().map(|m| m.num_devices).max().unwrap_or(0);
    Survey {
        members,
        expected,
        rejected: refused,
    }
}

/// Mount `plan`'s filesystem by the rules in this module's documentation.
pub fn mount_guarded(
    host: &dyn MemberHost,
    plan: &Plan<'_>,
    progress: &dyn ProgressCallback,
) -> Result<Outcome, Failure> {
    let label = plan.label;
    let plain = without_degraded(plan.mount_opts);
    let mut waited = Duration::ZERO;
    let mut last_mount_error: Option<String> = None;
    // The rejection is read from the primary copy before anything is
    // scanned or mounted; the witness, on the filesystem, is checked once it
    // can be read (after the mount). An unreadable primary cannot say who is
    // rejected: every job that writes is refused before anything is scanned
    // (review F1); a read-only job gets a read-only mount.
    let primary = read_copy(&primary_path(plan.record_dir, plan.fsid), plan.fsid);
    let rejected: BTreeSet<String> = match &primary {
        Copy::Read(r) => r.rejected.iter().map(|m| m.dev_uuid.clone()).collect(),
        Copy::Absent | Copy::Bad(_) => BTreeSet::new(),
    };
    if let Copy::Bad(e) = &primary {
        let why = format!(
            "the RAID record is unreadable ({e}): it cannot say which members are rejected or \
             which hold the last degraded period's writes — every job that writes the target is \
             refused until it is restored from a backup or repaired"
        );
        return match plan.intent {
            Intent::Write => Err(refused(why)),
            Intent::ReadOnly => {
                let s = survey(host, plan, &rejected, progress);
                mount_read_only_unvouched(host, plan, s, why, progress)
            }
        };
    }
    let found = loop {
        let s = survey(host, plan, &rejected, progress);
        if !s.rejected.is_empty() {
            return Err(refused(rejected_why(
                &s.rejected.iter().collect::<Vec<_>>(),
            )));
        }
        // Members whose superblock generations differ, whatever the primary
        // says (it may be a stale copy from before an accept; review round
        // 3): each member's own witness is read alone before anything is
        // mounted read-write, and a member any copy rejects is refused.
        if s.all_present()
            && generations_differ(&s.members)
            && let Some(refusal) = member_witness_check(host, plan, &s, &rejected, progress)
        {
            return refusal;
        }
        // Members whose superblock generations differ with no record to say
        // why (the primary lost to a rollback, or never written): one of them
        // may be a drive `raid accept` gave up, whose tree the kernel would
        // take. Never read-write (review F1).
        if s.all_present()
            && primary == Copy::Absent
            && let Some(why) = diverged_without_record(plan, &s.members)
        {
            return match plan.intent {
                Intent::Write => Err(refused(why)),
                Intent::ReadOnly => mount_read_only_unvouched(host, plan, s, why, progress),
            };
        }
        if s.all_present() {
            match host.mount(&plain, plan.fsid, plan.mount) {
                Ok(()) => break Ok(s),
                Err(e) => {
                    progress.on_log(
                        LogLevel::Warning,
                        &format!(
                            "Target '{label}': every member is present, but the mount without \
                             degraded failed: {e}"
                        ),
                    );
                    last_mount_error = Some(e);
                }
            }
        }
        if waited >= plan.wait {
            break Err(s);
        }
        if waited.is_zero() {
            progress.on_log(
                LogLevel::Info,
                &format!(
                    "Target '{label}': {} of {} members present — waiting up to {}s for the \
                     rest",
                    s.members.len(),
                    if s.expected == 0 {
                        "?".to_string()
                    } else {
                        s.expected.to_string()
                    },
                    plan.wait.as_secs()
                ),
            );
        }
        host.pause(POLL);
        waited += POLL;
    };

    let s = match found {
        Ok(s) => return after_normal_mount(host, plan, s, waited, progress),
        Err(s) => s,
    };
    if s.members.is_empty() {
        return Err(Failure::NoMember(format!(
            "no member of filesystem {} was found (blkid) after waiting {}s",
            plan.fsid,
            waited.as_secs()
        )));
    }
    if s.all_present() {
        // Every member present and the plain mount still fails: never
        // licensed to try degraded.
        return Err(refused(format!(
            "every member of {} is present, yet the mount without degraded failed ({}) — \
             not mounting it degraded",
            plan.fsid,
            last_mount_error.unwrap_or_default()
        )));
    }
    let absent_serials: Vec<String> = plan
        .serials
        .iter()
        .filter(|s| !host.serial_attached(s))
        .cloned()
        .collect();
    if !plan.serials.is_empty() && absent_serials.is_empty() {
        return Err(refused(format!(
            "only {} of {} members of {} were found, yet every configured drive ({}) is \
             attached — the kernel or a damaged superblock is hiding a member; not mounting \
             degraded",
            s.members.len(),
            s.expected,
            plan.fsid,
            plan.serials.join(", ")
        )));
    }
    mount_degraded(host, plan, s, absent_serials, progress)
}

/// Whether the members' superblock generations are not all one.
fn generations_differ(members: &[Member]) -> bool {
    members
        .first()
        .is_some_and(|f| members.iter().any(|m| m.generation != f.generation))
}

/// Devices forgotten for a one-member probe, registered again when it goes
/// out of scope, on every path. A registration that fails here leaves the
/// member unknown to the kernel: the plain mount then fails and the guard
/// waits or refuses, the cautious direction.
struct Reregister<'a> {
    host: &'a dyn MemberHost,
    paths: Vec<String>,
}

impl Drop for Reregister<'_> {
    fn drop(&mut self) {
        for path in self.paths.drain(..) {
            let _ = self.host.scan(&path);
        }
    }
}

/// Each present member's own witness, read with that member alone: every
/// other present member forgotten by its exact path, the member mounted
/// [`PROBE_OPTS`] at a private directory, the witness read, unmounted, the
/// others registered again. Refused when the filesystem is mounted anywhere
/// (nothing is forgotten or mounted then), when a probe fails, or when a
/// member's own copy cannot be read.
fn member_witnesses(
    host: &dyn MemberHost,
    plan: &Plan<'_>,
    members: &[Member],
) -> Result<Vec<Copy>, String> {
    match host.fsid_mounted(plan.fsid) {
        Ok(false) => {}
        Ok(true) => {
            return Err(format!(
                "filesystem {} is mounted elsewhere: its members, whose generations differ, \
                 cannot each be read alone — not mounting it",
                plan.fsid
            ));
        }
        Err(e) => {
            return Err(format!(
                "whether filesystem {} is mounted elsewhere cannot be read ({e})",
                plan.fsid
            ));
        }
    }
    let mut out = Vec::new();
    for m in members {
        let mut again = Reregister {
            host,
            paths: Vec::new(),
        };
        for other in members.iter().filter(|o| o.path != m.path) {
            again.paths.push(other.path.clone());
            host.forget(&other.path).map_err(|e| {
                format!(
                    "{} cannot be set aside to read {} alone ({e})",
                    other.path, m.path
                )
            })?;
        }
        let at = host.temp_mountpoint()?;
        if let Err(e) = host.mount_device(PROBE_OPTS, &m.path, &at) {
            let _ = host.remove_mountpoint(&at);
            return Err(format!(
                "{}: the read-only probe of this member alone failed ({e})",
                m.path
            ));
        }
        let copy = host.read_witness(&at, plan.fsid);
        host.umount(&at)
            .map_err(|e| format!("{}: the probe mount at {at} will not unmount ({e})", m.path))?;
        let _ = host.remove_mountpoint(&at);
        if let Copy::Bad(e) = &copy {
            return Err(format!(
                "{}: its own RAID record is unreadable ({e})",
                m.path
            ));
        }
        out.push(copy);
        drop(again);
    }
    Ok(out)
}

/// The per-member check of members whose generations differ: `None` to go
/// on, `Some` the job's outcome (a refusal, or a read-only job's mount of a
/// member no copy rejects).
fn member_witness_check(
    host: &dyn MemberHost,
    plan: &Plan<'_>,
    s: &Survey,
    primary_rejected: &BTreeSet<String>,
    progress: &dyn ProgressCallback,
) -> Option<Result<Outcome, Failure>> {
    let witnesses = match member_witnesses(host, plan, &s.members) {
        Ok(w) => w,
        Err(why) => {
            let why = format!(
                "{why} — the members' generations differ and each member's own record must be \
                 read before a read-write mount; restore the NEWEST copy of the RAID record, or \
                 pull the drive that is not wanted"
            );
            return Some(match plan.intent {
                Intent::Write => Err(refused(why)),
                // Mounted elsewhere: never a second mount of it.
                Intent::ReadOnly if why.contains("mounted elsewhere") => Err(refused(why)),
                Intent::ReadOnly => mount_read_only_unvouched(
                    host,
                    plan,
                    Survey {
                        members: s.members.clone(),
                        expected: s.expected,
                        rejected: Vec::new(),
                    },
                    why,
                    progress,
                ),
            });
        }
    };
    let mut rejected = primary_rejected.clone();
    for copy in &witnesses {
        if let Copy::Read(r) = copy {
            rejected.extend(r.rejected.iter().map(|m| m.dev_uuid.clone()));
        }
    }
    let found: Vec<&Member> = s
        .members
        .iter()
        .filter(|m| rejected.contains(&m.dev_uuid))
        .collect();
    if found.is_empty() {
        return None;
    }
    let why = format!(
        "{} (found by reading each member's own record; the primary copy may be older than the \
         accept — restore the NEWEST copy of the record)",
        rejected_why(&found)
    );
    Some(match plan.intent {
        Intent::Write => Err(refused(why)),
        Intent::ReadOnly => {
            // Only a member no copy rejects, alone: the rejected ones set
            // aside (left unregistered: they are refused anyway).
            let Some(keep) = s.members.iter().find(|m| !rejected.contains(&m.dev_uuid)) else {
                return Some(Err(refused(why)));
            };
            for r in &found {
                if let Err(e) = host.forget(&r.path) {
                    return Some(Err(refused(format!(
                        "{why}; and {} cannot be set aside ({e})",
                        r.path
                    ))));
                }
            }
            match host.mount_device(&stale_read_opts(plan.mount_opts), &keep.path, plan.mount) {
                Ok(()) => {
                    let outcome = Outcome::ReadOnly {
                        members: vec![keep.clone()],
                        why,
                    };
                    let (status, detail) = outcome.status_line();
                    progress.on_log(
                        LogLevel::Warning,
                        &format!("Target '{}': {status}: {detail}", plan.label),
                    );
                    Ok(outcome)
                }
                Err(e) => Err(Failure::MountFailed(format!(
                    "the read-only mount of {} failed: {e}; {why}",
                    keep.path
                ))),
            }
        }
    })
}

/// Why members whose superblock generations differ cannot be mounted
/// read-write when no record is readable, or `None` when they agree (a
/// healthy pair has one generation).
fn diverged_without_record(plan: &Plan<'_>, members: &[Member]) -> Option<String> {
    let first = members.first()?.generation;
    if members.iter().all(|m| m.generation == first) {
        return None;
    }
    Some(format!(
        "the members' superblock generations differ ({}) and no RAID record explains it \
         ({} is absent): one of them may lack writes the other holds, or be a drive 'btrdasd \
         raid accept' gave up — not mounting it read-write (each member's own record named no \
         rejection). Restore the NEWEST copy of the record from a backup, or, if one drive is \
         dead, pull it and run 'btrdasd raid accept {}'",
        members
            .iter()
            .map(|m| format!(
                "devid {} {} at {}: generation {}",
                m.devid, m.dev_uuid, m.path, m.generation
            ))
            .collect::<Vec<_>>()
            .join("; "),
        primary_path(plan.record_dir, plan.fsid).display(),
        plan.label
    ))
}

/// A read-only job's mount when no readable record can vouch for the members:
/// [`stale_read_opts`], nothing written.
fn mount_read_only_unvouched(
    host: &dyn MemberHost,
    plan: &Plan<'_>,
    s: Survey,
    why: String,
    progress: &dyn ProgressCallback,
) -> Result<Outcome, Failure> {
    if s.members.is_empty() {
        return Err(Failure::NoMember(format!(
            "no member of filesystem {} was found (blkid); {why}",
            plan.fsid
        )));
    }
    host.mount(&stale_read_opts(plan.mount_opts), plan.fsid, plan.mount)
        .map_err(|e| Failure::MountFailed(format!("the read-only mount failed: {e}; {why}")))?;
    let outcome = Outcome::ReadOnly {
        members: s.members,
        why,
    };
    let (status, detail) = outcome.status_line();
    progress.on_log(
        LogLevel::Warning,
        &format!("Target '{}': {status}: {detail}", plan.label),
    );
    Ok(outcome)
}

fn after_normal_mount(
    host: &dyn MemberHost,
    plan: &Plan<'_>,
    s: Survey,
    waited: Duration,
    progress: &dyn ProgressCallback,
) -> Result<Outcome, Failure> {
    let take_down = |why: String| match host.umount(plan.mount) {
        Ok(()) => refused(why),
        Err(e) => Failure::Refused {
            why: format!("{why}; and it will not unmount: {e}"),
            left_mounted: true,
        },
    };
    match host.missing_devids(plan.fsid) {
        Ok(missing) if missing.is_empty() => {}
        Ok(missing) => {
            return Err(take_down(format!(
                "mounted, but the kernel reports devid(s) {missing:?} of {} missing while every \
                 member is attached — refusing to write to it",
                plan.fsid
            )));
        }
        Err(e) => {
            return Err(take_down(format!(
                "mounted, but whether a member is missing cannot be read ({e}) — refusing to \
                 write to it"
            )));
        }
    }
    let primary_path = primary_path(plan.record_dir, plan.fsid);
    let primary = read_copy(&primary_path, plan.fsid);
    let witness = host.read_witness(plan.mount, plan.fsid);
    // A rejection only the witness carries (the primary lost): seen only now.
    for copy in [&primary, &witness] {
        if let Copy::Read(r) = copy
            && let Some(why) = rejected_present(r, &s.members)
        {
            return Err(take_down(why));
        }
    }
    let (owed, record_problem, rejoined_now) = match rejoin(&primary, &witness, host.now()) {
        Rejoin::Clean => (None, None, false),
        Rejoin::Unreadable(e) => (None, Some(format!("{e}; nothing changed")), false),
        Rejoin::Owed {
            record,
            write_primary,
            write_witness,
            rejoined_now,
        } => {
            let mut problems = Vec::new();
            let read_only = is_read_only(plan.mount_opts);
            if !read_only {
                if write_primary && let Err(e) = write_copy(&primary_path, &record, true) {
                    problems.push(e);
                }
                if write_witness && let Err(e) = host.write_witness(plan.mount, &record) {
                    problems.push(e);
                }
            }
            // The rejoin counts only once it is recorded: a scrub begun
            // after it then pays the debt.
            let recorded = !read_only && problems.is_empty();
            (
                Some(record),
                (!problems.is_empty()).then(|| problems.join("; ")),
                rejoined_now && recorded,
            )
        }
    };
    let outcome = Outcome::Normal {
        members: s.members,
        waited,
        owed,
        record_problem,
        rejoined_now,
    };
    let (status, detail) = outcome.status_line();
    progress.on_log(
        if status == "OK" {
            LogLevel::Info
        } else {
            LogLevel::Warning
        },
        &format!("Target '{}': {status}: {detail}", plan.label),
    );
    Ok(outcome)
}

fn mount_degraded(
    host: &dyn MemberHost,
    plan: &Plan<'_>,
    s: Survey,
    absent_serials: Vec<String>,
    progress: &dyn ProgressCallback,
) -> Result<Outcome, Failure> {
    // The witness is on the filesystem: read it through a mount that writes
    // nothing, and take that mount down before deciding.
    host.mount(PROBE_OPTS, plan.fsid, plan.mount).map_err(|e| {
        Failure::MountFailed(format!(
            "{} of {} members present; the read-only probe mount failed: {e}",
            s.members.len(),
            s.expected
        ))
    })?;
    let witness = host.read_witness(plan.mount, plan.fsid);
    host.umount(plan.mount).map_err(|e| Failure::Refused {
        why: format!("the read-only probe mount will not unmount: {e}"),
        left_mounted: true,
    })?;
    let primary_path = primary_path(plan.record_dir, plan.fsid);
    let primary = read_copy(&primary_path, plan.fsid);
    let record = match degraded_verdict(plan.fsid, &primary, &witness, &s.members, host.now()) {
        Verdict::Allow(r) => r,
        Verdict::SplitBrain { record, why } => {
            return match plan.intent {
                Intent::Write => Err(refused(why)),
                Intent::ReadOnly => {
                    mount_stale_read_only(host, plan, s, absent_serials, record, progress)
                }
            };
        }
        Verdict::Refuse(why) => return Err(refused(why)),
    };
    let read_only = is_read_only(plan.mount_opts);
    if !read_only {
        write_copy(&primary_path, &record, true).map_err(|e| {
            refused(format!(
                "the degraded period cannot be recorded ({e}) — not mounting degraded"
            ))
        })?;
    }
    host.mount(&with_degraded(plan.mount_opts), plan.fsid, plan.mount)
        .map_err(|e| Failure::MountFailed(format!("the degraded mount failed: {e}")))?;
    if !read_only && let Err(e) = host.write_witness(plan.mount, &record) {
        let why = format!(
            "the witness record cannot be written to the filesystem ({e}) — not using the \
             degraded mount"
        );
        return Err(match host.umount(plan.mount) {
            Ok(()) => refused(why),
            Err(u) => Failure::Refused {
                why: format!("{why}; and it will not unmount: {u}"),
                left_mounted: true,
            },
        });
    }
    let outcome = Outcome::Degraded {
        members: s.members,
        absent_serials,
        expected: s.expected,
        record,
        recorded: !read_only,
    };
    let (status, detail) = outcome.status_line();
    progress.on_log(
        LogLevel::Warning,
        &format!("Target '{}': {status}: {detail}", plan.label),
    );
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// The end of a job (bd DAS-Backup-Manager-su1b.14)
// ---------------------------------------------------------------------------

/// What [`check_before_unmount`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndCheck {
    /// No member missing: nothing to record.
    Whole,
    /// A member is missing, or may be: a degraded period is recorded for the
    /// present members (`why` says which, and why).
    Recorded { record: Record, why: String },
    /// A member is missing, or may be, and nothing could be recorded.
    Problem(String),
}

/// At the end of a job that mounted `plan`'s target read-write, before it is
/// unmounted: the kernel's view (`missing_devids`). A member missing — it
/// dropped while the job ran, and every write since went to the others alone
/// — or a view that cannot be read (possibly dropped: the cautious reading) is
/// recorded as a degraded period on the members still present, both copies,
/// as the guard records a degraded mount, so the next mount with every member
/// owes a scrub. The members present are found by filesystem UUID and read by
/// their superblocks; nothing is scanned or mounted. A record naming other
/// members (their period not yet scrubbed) is replaced by a period on the
/// present ones — the writes just made exist only there — and said. A copy
/// that cannot be read is a [`EndCheck::Problem`], never overwritten.
pub fn check_before_unmount(host: &dyn MemberHost, plan: &Plan<'_>) -> EndCheck {
    let (missing, why) = match host.missing_devids(plan.fsid) {
        Ok(m) if m.is_empty() => return EndCheck::Whole,
        Ok(m) => (
            Some(m.clone()),
            format!("the kernel shows devid(s) {m:?} missing at the end of the job"),
        ),
        Err(e) => (
            None,
            format!(
                "whether a member dropped during the job cannot be read ({e}); taken as \
                 possibly dropped"
            ),
        ),
    };
    let paths = match host.members(plan.fsid) {
        Ok(p) => p,
        Err(e) => {
            return EndCheck::Problem(format!(
                "{why}, and the members cannot be listed ({e}): nothing recorded"
            ));
        }
    };
    let present: Vec<Member> = paths
        .iter()
        .filter_map(|path| {
            host.read_super(path)
                .and_then(|t| parse_super(path, &t, plan.fsid))
                .ok()
        })
        .filter(|m| missing.as_ref().is_none_or(|ms| !ms.contains(&m.devid)))
        .fold(Vec::new(), |mut acc: Vec<Member>, m| {
            if !acc.iter().any(|o| o.devid == m.devid) {
                acc.push(m);
            }
            acc
        });
    if present.is_empty() {
        return EndCheck::Problem(format!(
            "{why}, and no present member can be identified: nothing recorded"
        ));
    }
    let primary_path = primary_path(plan.record_dir, plan.fsid);
    let primary = read_copy(&primary_path, plan.fsid);
    let witness = host.read_witness(plan.mount, plan.fsid);
    let now = host.now();
    let (record, why) = match degraded_verdict(plan.fsid, &primary, &witness, &present, now) {
        Verdict::Allow(r) => (r, why),
        Verdict::SplitBrain { record, .. } => (
            Record {
                fsid: plan.fsid.to_ascii_lowercase(),
                state: State::Degraded,
                members: present
                    .iter()
                    .map(|m| PeriodMember {
                        devid: m.devid,
                        dev_uuid: m.dev_uuid.clone(),
                        generation: m.generation,
                    })
                    .collect(),
                since: now,
                last: now,
                rejoined: None,
                rejected: record.rejected.clone(),
            },
            format!(
                "{why}; the earlier degraded period on {} was not yet scrubbed, so both \
                 drives now hold writes the other lacks",
                names(&record.dev_uuids())
            ),
        ),
        Verdict::Refuse(e) => {
            return EndCheck::Problem(format!("{why}, and {e}: nothing recorded"));
        }
    };
    if let Err(e) = write_copy(&primary_path, &record, true) {
        return EndCheck::Problem(format!("{why}, and the record cannot be written ({e})"));
    }
    if let Err(e) = host.write_witness(plan.mount, &record) {
        return EndCheck::Problem(format!(
            "{why}; the record is written, the witness is not ({e})"
        ));
    }
    EndCheck::Recorded { record, why }
}

/// The one line `btrdasd backup raid-check` prints for `backup-run.sh`:
/// `RAID CHECK: <STATUS>  (<detail>)`, STATUS `OK`, `RECORDED` or `WARN`.
pub fn end_check_line(check: &EndCheck) -> String {
    let (status, detail) = match check {
        EndCheck::Whole => ("OK", "no member missing at the end of the job".to_string()),
        EndCheck::Recorded { record, why } => (
            "RECORDED",
            format!(
                "{why}: a degraded period is recorded on {} — a scrub is owed once every \
                 member is back",
                names(&record.dev_uuids())
            ),
        ),
        EndCheck::Problem(why) => ("WARN", why.clone()),
    };
    format!("RAID CHECK: {status}  ({})", detail.replace('\n', " "))
}

// ---------------------------------------------------------------------------
// The owed scrub, started after the job (bd DAS-Backup-Manager-su1b.14)
// ---------------------------------------------------------------------------

/// The unit an owed scrub is started with. It scrubs every `[scrub].targets`
/// filesystem in order (`btrdasd scrub run` takes no target): the owed one is
/// among them.
pub const SCRUB_UNIT: &str = "das-scrub.service";

/// Targets whose mount in this process was the rejoin ([`Outcome::Normal`]
/// `rejoined_now`), waiting for the job's maintenance lock to be released.
static OWED_SCRUB: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Note that `label`'s mount was the rejoin: the scrub owed starts once this
/// process lets the maintenance lock go ([`after_maintenance_released`]).
pub fn note_owed_scrub(label: &str) {
    let mut owed = OWED_SCRUB.lock().unwrap_or_else(|e| e.into_inner());
    if !owed.iter().any(|l| l == label) {
        owed.push(label.to_string());
    }
}

/// The targets noted and not yet started for, taken.
pub fn take_owed_scrub() -> Vec<String> {
    std::mem::take(&mut *OWED_SCRUB.lock().unwrap_or_else(|e| e.into_inner()))
}

/// `systemctl start --no-block das-scrub.service`: queued, never waited for,
/// so nothing here can wait on the locks the scrub needs. A backup still
/// preempts it through the claim. Refused in a test build: a test never
/// starts a real unit.
pub fn start_owed_scrub() -> Result<(), String> {
    if cfg!(test) || cfg!(feature = "test-overrides") {
        return Err(format!(
            "refused in a test build: a test never starts {SCRUB_UNIT}"
        ));
    }
    let out = std::process::Command::new("systemctl")
        .args(["start", "--no-block", SCRUB_UNIT])
        .output()
        .map_err(|e| format!("systemctl could not be run: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl start --no-block {SCRUB_UNIT} exited with code {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Called by [`crate::maintenance::MaintenanceHeld`] right after it releases
/// a lock this process took: starts the owed scrub for anything noted, and
/// says so on stderr (the journal of a unit or the helper). A start that
/// fails is a warning, never an error. Returns what it said, for tests.
pub fn after_maintenance_released() -> Option<String> {
    let owed = take_owed_scrub();
    if owed.is_empty() {
        return None;
    }
    let line = match start_owed_scrub() {
        Ok(()) => format!(
            "info: RAID-1 target(s) {} rejoined: {SCRUB_UNIT} started (not waited for) to pay \
             the scrub owed",
            owed.join(", ")
        ),
        Err(e) => format!(
            "warning: RAID-1 target(s) {} rejoined, and the owed scrub was not started ({e}); \
             run 'btrdasd scrub run' or wait for the scheduled one",
            owed.join(", ")
        ),
    };
    eprintln!("{line}");
    Some(line)
}

// ---------------------------------------------------------------------------
// `btrdasd raid accept` (bd DAS-Backup-Manager-su1b.11)
// ---------------------------------------------------------------------------

/// What `raid accept` found: the guard refuses this target for split brain
/// right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitBrainNow {
    /// The members present, each by its own superblock.
    pub present: Vec<Member>,
    /// The last degraded period, which ran on members that are absent.
    pub period: Record,
    pub absent_serials: Vec<String>,
    pub expected: u64,
    /// An earlier accept wrote one copy and not the other: this one
    /// finishes it.
    pub resumed: bool,
}

/// What an accept did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The period given up.
    pub before: Record,
    /// The record written to both copies.
    pub after: Record,
    /// What was shown before the label was typed.
    pub explanation: String,
}

/// Look at the target as the guard would, once, without waiting and without
/// mounting anything writable: `Ok` only when the guard would refuse it for
/// split brain now — fewer members present than the filesystem has, a
/// configured drive absent, and the last degraded period run on members
/// that are not present. The witness is read through the read-only probe
/// mount ([`PROBE_OPTS`]), taken down before deciding. A member the record
/// already rejects is refused, so two drives can never be accepted back and
/// forth.
pub fn split_brain_now(
    host: &dyn MemberHost,
    plan: &Plan<'_>,
    progress: &dyn ProgressCallback,
) -> Result<SplitBrainNow, String> {
    let primary_path = primary_path(plan.record_dir, plan.fsid);
    let primary = read_copy(&primary_path, plan.fsid);
    let rejected: BTreeSet<String> = match &primary {
        Copy::Read(r) => r.rejected.iter().map(|m| m.dev_uuid.clone()).collect(),
        Copy::Absent | Copy::Bad(_) => BTreeSet::new(),
    };
    let s = survey(host, plan, &rejected, progress);
    if !s.rejected.is_empty() {
        return Err(rejected_why(&s.rejected.iter().collect::<Vec<_>>()));
    }
    if s.members.is_empty() {
        return Err(format!(
            "no member of filesystem {} is present: nothing to accept",
            plan.fsid
        ));
    }
    if s.all_present() {
        return Err(format!(
            "every member of {} is present ({}): the guard mounts it normally; nothing to \
             accept",
            plan.fsid,
            member_list(&s.members)
        ));
    }
    let absent_serials: Vec<String> = plan
        .serials
        .iter()
        .filter(|serial| !host.serial_attached(serial))
        .cloned()
        .collect();
    if !plan.serials.is_empty() && absent_serials.is_empty() {
        return Err(format!(
            "every configured drive ({}) is attached although a member is not found: the \
             guard refuses this for another reason than split brain; nothing to accept",
            plan.serials.join(", ")
        ));
    }
    host.mount(PROBE_OPTS, plan.fsid, plan.mount)
        .map_err(|e| format!("the read-only probe mount failed: {e}"))?;
    let witness = host.read_witness(plan.mount, plan.fsid);
    host.umount(plan.mount)
        .map_err(|e| format!("the read-only probe mount will not unmount: {e}"))?;
    let found = |period: Record, resumed: bool| SplitBrainNow {
        present: s.members.clone(),
        period,
        absent_serials: absent_serials.clone(),
        expected: s.expected,
        resumed,
    };
    match degraded_verdict(plan.fsid, &primary, &witness, &s.members, host.now()) {
        Verdict::SplitBrain { record, .. } => Ok(found(record, false)),
        Verdict::Allow(_) => Err(format!(
            "not refused: the guard mounts it degraded on the members present ({}); nothing \
             to accept",
            member_list(&s.members)
        )),
        Verdict::Refuse(why) => match half_accepted(&primary, &witness, &s.members) {
            Some(period) => Ok(found(period, true)),
            None => Err(format!(
                "the guard refuses this target for another reason than split brain: {why}"
            )),
        },
    }
}

/// An accept that wrote one copy and not the other: one copy names exactly
/// the present members and rejects every member the other copy's period ran
/// on. Returns that other copy (the period given up).
fn half_accepted(primary: &Copy, witness: &Copy, present: &[Member]) -> Option<Record> {
    let (Copy::Read(a), Copy::Read(b)) = (primary, witness) else {
        return None;
    };
    let present: BTreeSet<&str> = present.iter().map(|m| m.dev_uuid.as_str()).collect();
    let finishes = |done: &Record, old: &Record| {
        done.dev_uuids() == present && old.dev_uuids().is_subset(&done.rejected_uuids())
    };
    if finishes(a, b) {
        Some(b.clone())
    } else if finishes(b, a) {
        Some(a.clone())
    } else {
        None
    }
}

/// The record an accept writes: a degraded period on the present members,
/// from `now`, every member of the period given up (and any already
/// rejected) rejected.
pub fn accepted_record(found: &SplitBrainNow, now: i64) -> Record {
    let mut rejected = found.period.rejected.clone();
    for m in &found.period.members {
        if !found.present.iter().any(|p| p.dev_uuid == m.dev_uuid)
            && !rejected.iter().any(|r| r.dev_uuid == m.dev_uuid)
        {
            rejected.push(m.clone());
        }
    }
    Record {
        fsid: found.period.fsid.clone(),
        state: State::Degraded,
        members: found
            .present
            .iter()
            .map(|m| PeriodMember {
                devid: m.devid,
                dev_uuid: m.dev_uuid.clone(),
                generation: m.generation,
            })
            .collect(),
        since: now,
        last: now,
        rejoined: None,
        rejected,
    }
}

/// What the operator is shown before typing the label. `runs` is how many
/// backup runs the history records inside the period given up: `None` or an
/// error is said as unknown, never as 0.
pub fn accept_explanation(
    label: &str,
    fsid: &str,
    found: &SplitBrainNow,
    runs: &Result<Option<u64>, String>,
) -> String {
    let p = &found.period;
    let runs = match runs {
        Ok(Some(n)) => format!("{n} backup run(s) recorded in the history"),
        Ok(None) => "unknown (no backup history)".to_string(),
        Err(e) => format!("unknown ({e})"),
    };
    let mut text = format!(
        "Target '{label}' (filesystem {fsid}) is refused for split brain.\n\
         \n\
         Present ({} of {} members):\n",
        found.present.len(),
        found.expected
    );
    for m in &found.present {
        text.push_str(&format!(
            "  devid {} {} at {}, superblock generation {}\n",
            m.devid, m.dev_uuid, m.path, m.generation
        ));
    }
    text.push_str("\nThe last degraded period ran on:\n");
    for m in &p.members {
        text.push_str(&format!(
            "  devid {} {}, superblock generation {} when the period was recorded\n",
            m.devid, m.dev_uuid, m.generation
        ));
    }
    text.push_str(&format!(
        "  from {} to {}{}; absent drive(s): {}\n\
         \n\
         Writes of that period exist only on the absent drive: {runs} between those times.\n\
         The present drive's data stops no later than {}.\n\
         {}\
         \n\
         Accepting makes the present drive the filesystem's history from now on. What \
         only the absent drive holds is given up, and the absent drive is REFUSED from then \
         on, alone or beside this one, until it is wiped (wipefs) or removed from the \
         filesystem (btrfs device remove, btrfs replace). The present drive is mounted \
         degraded to write the record, and every later job runs on it degraded, one copy of \
         every write, until a second drive is added (btrfs replace) and scrubbed.\n",
        when(p.since),
        when(p.last),
        match (p.state, p.rejoined) {
            (State::ScrubOwed, Some(r)) => format!(
                ", every member present again from {} (the scrub owed did not complete)",
                when(r)
            ),
            _ => String::new(),
        },
        if found.absent_serials.is_empty() {
            "unknown (no serial configured)".to_string()
        } else {
            found.absent_serials.join(", ")
        },
        when(p.since),
        if found.resumed {
            "(An earlier accept wrote only one copy of its record; this one finishes it.)\n"
        } else {
            ""
        },
    ));
    text
}

/// The whole accept, under the caller's locks: [`split_brain_now`], the
/// explanation handed to `confirm`, which returns what the operator typed
/// (an `Err` when it cannot ask: never a yes), the label compared exactly,
/// then the members looked at again (any other present: refused), the
/// present member mounted degraded and read-write, the kernel's view checked
/// (another member present: unmounted, refused), the primary copy and the
/// witness written, and unmounted. Nothing is written unless
/// the typed text is the label. `runs_between(since, last)` counts the
/// backup runs the history records in the period given up.
pub fn accept(
    host: &dyn MemberHost,
    plan: &Plan<'_>,
    runs_between: &dyn Fn(i64, i64) -> Result<Option<u64>, String>,
    confirm: &mut dyn FnMut(&str) -> Result<String, String>,
    progress: &dyn ProgressCallback,
) -> Result<Accepted, String> {
    let found = split_brain_now(host, plan, progress)?;
    let explanation = accept_explanation(
        plan.label,
        plan.fsid,
        &found,
        &runs_between(found.period.since, found.period.last),
    );
    let typed = confirm(&explanation).map_err(|e| format!("not accepted: {e}"))?;
    let typed = typed.strip_suffix('\n').unwrap_or(&typed);
    let typed = typed.strip_suffix('\r').unwrap_or(typed);
    if typed != plan.label {
        return Err(format!(
            "not accepted: '{typed}' is not the target label '{}' — nothing changed",
            plan.label
        ));
    }
    // Look again (review F2): a drive plugged in while the label was typed
    // is registered by udev on its own, and the mount below would assemble
    // it. Only the members the explanation named may be present.
    let accepted_uuids: BTreeSet<&str> =
        found.present.iter().map(|m| m.dev_uuid.as_str()).collect();
    // Every listed path must be read: one whose superblock cannot be read is
    // a refusal, never left out (review round 3).
    let now_present: Vec<Member> = host
        .members(plan.fsid)
        .map_err(|e| format!("not accepted, nothing changed: cannot list the members ({e})"))?
        .iter()
        .map(|path| {
            host.read_super(path)
                .and_then(|t| parse_super(path, &t, plan.fsid))
                .map_err(|e| {
                    format!(
                        "not accepted, nothing changed: {path} is listed with this filesystem \
                         and its superblock cannot be read ({e}) — unplug that drive first"
                    )
                })
        })
        .collect::<Result<_, _>>()?;
    let foreign: Vec<String> = now_present
        .iter()
        .filter(|m| !accepted_uuids.contains(m.dev_uuid.as_str()))
        .map(|m| format!("devid {} ({}) at {}", m.devid, m.dev_uuid, m.path))
        .collect();
    if !foreign.is_empty() {
        return Err(format!(
            "not accepted, nothing changed: {} is attached as well — unplug it first, then run \
             'btrdasd raid accept {}' again",
            foreign.join(", "),
            plan.label
        ));
    }
    // The absent drive attached again, whatever blkid lists.
    let back: Vec<&String> = found
        .absent_serials
        .iter()
        .filter(|serial| host.serial_attached(serial))
        .collect();
    if !back.is_empty() {
        return Err(format!(
            "not accepted, nothing changed: drive {} is attached again — unplug it first, then \
             run 'btrdasd raid accept {}' again",
            back.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            plan.label
        ));
    }
    if !accepted_uuids
        .iter()
        .all(|u| now_present.iter().any(|m| m.dev_uuid == *u))
    {
        return Err(
            "not accepted, nothing changed: the drive to accept is no longer present".into(),
        );
    }
    let after = accepted_record(&found, host.now());
    let primary_path = primary_path(plan.record_dir, plan.fsid);
    host.mount(
        &with_degraded(&without_degraded(plan.mount_opts)),
        plan.fsid,
        plan.mount,
    )
    .map_err(|e| format!("not accepted, nothing changed: the degraded mount failed ({e})"))?;
    let take_down = |why: String| match host.umount(plan.mount) {
        Ok(()) => why,
        Err(u) => format!("{why}; and {} will not unmount: {u}", plan.mount),
    };
    // The kernel's own view of what it assembled: only the accepted members
    // present (review F2), or the mount comes straight down, nothing written.
    let accepted_devids: BTreeSet<u64> = found.present.iter().map(|m| m.devid).collect();
    match host.devinfo(plan.fsid) {
        Ok(devs) => {
            let extra: Vec<u64> = devs
                .iter()
                .filter(|(id, missing)| !missing && !accepted_devids.contains(id))
                .map(|(id, _)| *id)
                .collect();
            if !extra.is_empty() {
                return Err(take_down(format!(
                    "not accepted, nothing changed: the mounted filesystem has devid {} present \
                     besides the drive to accept — unmounted; unplug it first",
                    extra
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", devid ")
                )));
            }
        }
        Err(e) => {
            return Err(take_down(format!(
                "not accepted, nothing changed: what the kernel assembled cannot be read ({e}) \
                 — unmounted"
            )));
        }
    }
    if let Err(e) = write_copy(&primary_path, &after, true) {
        return Err(take_down(format!(
            "not accepted, nothing changed: the record cannot be written ({e})"
        )));
    }
    let witness = host.write_witness(plan.mount, &after);
    let unmounted = host.umount(plan.mount);
    match (witness, unmounted) {
        (Ok(()), Ok(())) => {}
        (Err(e), u) => {
            return Err(format!(
                "the record is written, but the witness is not ({e}){}; run 'btrdasd raid \
                 accept' again to finish",
                u.err()
                    .map(|u| format!("; and {} will not unmount: {u}", plan.mount))
                    .unwrap_or_default()
            ));
        }
        (Ok(()), Err(u)) => {
            return Err(format!(
                "accepted (both copies written), but {} will not unmount: {u}",
                plan.mount
            ));
        }
    }
    Ok(Accepted {
        before: found.period,
        after,
        explanation,
    })
}

/// A read-only job's mount of a split-brain member: [`stale_read_opts`],
/// nothing written, the record left as it is.
fn mount_stale_read_only(
    host: &dyn MemberHost,
    plan: &Plan<'_>,
    s: Survey,
    absent_serials: Vec<String>,
    record: Record,
    progress: &dyn ProgressCallback,
) -> Result<Outcome, Failure> {
    host.mount(&stale_read_opts(plan.mount_opts), plan.fsid, plan.mount)
        .map_err(|e| Failure::MountFailed(format!("the read-only mount failed: {e}")))?;
    let outcome = Outcome::StaleReadOnly {
        members: s.members,
        absent_serials,
        expected: s.expected,
        record,
    };
    let (status, detail) = outcome.status_line();
    progress.on_log(
        LogLevel::Warning,
        &format!("Target '{}': {status}: {detail}", plan.label),
    );
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::TestProgress;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    const FS: &str = "5eed0016-0000-4000-8000-000000000016";
    const DX: &str = "5eed00a1-0000-4000-8000-0000000000a1";
    const DY: &str = "5eed00a2-0000-4000-8000-0000000000a2";

    fn sup(devid: u64, dev_uuid: &str, generation: u64) -> String {
        format!(
            "superblock: bytenr=65536, device=/dev/x\n\
             ---------------------------------------------------------\n\
             csum_type\t\t0 (crc32c)\n\
             magic\t\t\t_BHRfS_M [match]\n\
             fsid\t\t\t{FS}\n\
             metadata_uuid\t\t00000000-0000-0000-0000-000000000000\n\
             generation\t\t{generation}\n\
             chunk_root_generation\t7\n\
             num_devices\t\t2\n\
             cache_generation\t0\n\
             dev_item.uuid\t\t{dev_uuid}\n\
             dev_item.fsid\t\t{FS} [match]\n\
             dev_item.devid\t\t{devid}\n\
             dev_item.generation\t0\n"
        )
    }

    /// A described host: drives that are attached (serial → partition and
    /// superblock), when each first appears (poll count), and a witness file
    /// per set of present members (each member's view of the filesystem).
    struct FakeHost {
        /// serial, partition, devid, dev_uuid, generation, first poll it appears
        drives: Vec<(&'static str, &'static str, u64, &'static str, u64, u32)>,
        /// Partitions blkid lists although no configured serial is attached
        /// (the kernel's view lost, the serial table says otherwise).
        polls: Cell<u32>,
        calls: RefCell<Vec<String>>,
        mounted: RefCell<Option<String>>,
        /// A witness per view (sorted dev_uuids present).
        witness: RefCell<BTreeMap<String, Copy>>,
        missing: RefCell<Result<Vec<u64>, String>>,
        /// The kernel's devinfo, when a case says it (else the present
        /// drives, each missing as `missing` says).
        devinfo: RefCell<Option<DevInfo>>,
        /// The member a device mount mounted alone: its own view.
        alone: RefCell<Option<String>>,
        /// Device paths whose ro mount fails, and whose superblock cannot be
        /// read.
        fail_device_mount: RefCell<BTreeSet<String>>,
        bad_super: RefCell<BTreeSet<String>>,
        /// The filesystem is mounted elsewhere.
        mounted_elsewhere: Cell<bool>,
        fail_mount_plain: Cell<bool>,
        hide_members: Cell<bool>,
        /// The kernel as the incident left it (a `--forget`): a member is
        /// known to btrfs only once scanned, and a mount without degraded
        /// fails (rc 32, VM-proven) while any present member is not. The
        /// first `scan_failures` scans of /dev/sdc1 fail.
        model_kernel: Cell<bool>,
        scan_failures: Cell<u32>,
        registered: RefCell<BTreeSet<String>>,
        now: Cell<i64>,
    }

    /// What the kernel lists for a mounted filesystem: devid, missing.
    type DevInfo = Result<Vec<(u64, bool)>, String>;

    impl FakeHost {
        fn new(drives: Vec<(&'static str, &'static str, u64, &'static str, u64, u32)>) -> Self {
            Self {
                drives,
                polls: Cell::new(0),
                calls: RefCell::new(Vec::new()),
                mounted: RefCell::new(None),
                witness: RefCell::new(BTreeMap::new()),
                missing: RefCell::new(Ok(Vec::new())),
                devinfo: RefCell::new(None),
                alone: RefCell::new(None),
                fail_device_mount: RefCell::new(BTreeSet::new()),
                bad_super: RefCell::new(BTreeSet::new()),
                mounted_elsewhere: Cell::new(false),
                fail_mount_plain: Cell::new(false),
                hide_members: Cell::new(false),
                model_kernel: Cell::new(false),
                scan_failures: Cell::new(0),
                registered: RefCell::new(BTreeSet::new()),
                now: Cell::new(1000),
            }
        }
        fn present(&self) -> Vec<&(&'static str, &'static str, u64, &'static str, u64, u32)> {
            self.drives
                .iter()
                .filter(|d| d.5 <= self.polls.get())
                .collect()
        }
        fn view(&self) -> String {
            let mut v: Vec<&str> = self.present().iter().map(|d| d.3).collect();
            v.sort_unstable();
            v.join("+")
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
        fn log(&self, c: String) {
            self.calls.borrow_mut().push(c);
        }
    }

    impl MemberHost for FakeHost {
        fn members(&self, fsid: &str) -> Result<Vec<String>, String> {
            self.log(format!("blkid {fsid}"));
            if self.hide_members.get() {
                return Ok(self
                    .present()
                    .iter()
                    .take(1)
                    .map(|d| d.1.to_string())
                    .collect());
            }
            Ok(self.present().iter().map(|d| d.1.to_string()).collect())
        }
        fn scan(&self, path: &str) -> Result<(), String> {
            self.log(format!("scan {path}"));
            if path == "/dev/sdc1" && self.scan_failures.get() > 0 {
                self.scan_failures.set(self.scan_failures.get() - 1);
                return Err("ERROR: device scan failed (fake)".into());
            }
            self.registered.borrow_mut().insert(path.to_string());
            Ok(())
        }
        fn forget(&self, path: &str) -> Result<(), String> {
            self.log(format!("forget {path}"));
            self.registered.borrow_mut().remove(path);
            Ok(())
        }
        fn mount_device(&self, opts: &str, device: &str, at: &str) -> Result<(), String> {
            self.log(format!("mount-dev {opts} {device} {at}"));
            if self.fail_device_mount.borrow().contains(device) {
                return Err("wrong fs type (fake)".into());
            }
            let uuid = self.drives.iter().find(|d| d.1 == device).unwrap().3;
            *self.alone.borrow_mut() = Some(uuid.to_string());
            *self.mounted.borrow_mut() = Some(opts.to_string());
            Ok(())
        }
        fn temp_mountpoint(&self) -> Result<String, String> {
            Ok("/run/fake-probe".into())
        }
        fn remove_mountpoint(&self, at: &str) -> Result<(), String> {
            self.log(format!("rmdir {at}"));
            Ok(())
        }
        fn fsid_mounted(&self, _fsid: &str) -> Result<bool, String> {
            Ok(self.mounted_elsewhere.get())
        }
        fn read_super(&self, path: &str) -> Result<String, String> {
            if self.bad_super.borrow().contains(path) {
                return Err(format!("{path}: bad superblock (fake)"));
            }
            let d = self.drives.iter().find(|d| d.1 == path).unwrap();
            Ok(sup(d.2, d.3, d.4))
        }
        fn serial_attached(&self, serial: &str) -> bool {
            self.present().iter().any(|d| d.0 == serial)
        }
        fn mount(&self, opts: &str, fsid: &str, mount: &str) -> Result<(), String> {
            self.log(format!("mount {opts} {fsid} {mount}"));
            let unknown_member = self.model_kernel.get()
                && self
                    .present()
                    .iter()
                    .any(|d| !self.registered.borrow().contains(d.1));
            if !opts.contains("degraded") && (self.fail_mount_plain.get() || unknown_member) {
                return Err("wrong fs type".into());
            }
            *self.mounted.borrow_mut() = Some(opts.to_string());
            Ok(())
        }
        fn umount(&self, mount: &str) -> Result<(), String> {
            self.log(format!("umount {mount}"));
            *self.mounted.borrow_mut() = None;
            *self.alone.borrow_mut() = None;
            Ok(())
        }
        fn missing_devids(&self, _fsid: &str) -> Result<Vec<u64>, String> {
            self.missing.borrow().clone()
        }
        fn devinfo(&self, _fsid: &str) -> Result<Vec<(u64, bool)>, String> {
            if let Some(d) = self.devinfo.borrow().clone() {
                return d;
            }
            let missing = self.missing.borrow().clone()?;
            Ok(self
                .present()
                .iter()
                .map(|d| (d.2, missing.contains(&d.2)))
                .collect())
        }
        fn read_witness(&self, _mount: &str, _fsid: &str) -> Copy {
            assert!(
                self.mounted.borrow().is_some(),
                "witness read while unmounted"
            );
            let view = self.alone.borrow().clone().unwrap_or_else(|| self.view());
            self.witness
                .borrow()
                .get(&view)
                .cloned()
                .unwrap_or(Copy::Absent)
        }
        fn write_witness(&self, _mount: &str, record: &Record) -> Result<(), String> {
            let opts = self
                .mounted
                .borrow()
                .clone()
                .expect("witness written while unmounted");
            assert!(
                !opts.split(',').any(|o| o == "ro"),
                "witness written to a ro mount"
            );
            self.log("write witness".into());
            self.witness
                .borrow_mut()
                .insert(self.view(), Copy::Read(record.clone()));
            Ok(())
        }
        fn pause(&self, how_long: Duration) {
            assert_eq!(how_long, POLL);
            self.polls.set(self.polls.get() + 1);
            self.now.set(self.now.get() + 5);
        }
        fn now(&self) -> i64 {
            self.now.get()
        }
    }

    struct Case {
        dir: tempfile::TempDir,
        serials: Vec<String>,
    }

    impl Case {
        fn new() -> Self {
            Self {
                dir: tempfile::TempDir::new().unwrap(),
                serials: vec!["SERIAL11".into(), "SERIAL12".into()],
            }
        }
        fn run(&self, host: &FakeHost) -> (Result<Outcome, Failure>, TestProgress) {
            self.run_as(host, Intent::Write)
        }
        fn run_as(
            &self,
            host: &FakeHost,
            intent: Intent,
        ) -> (Result<Outcome, Failure>, TestProgress) {
            let progress = TestProgress::default();
            let plan = Plan {
                label: "primary",
                fsid: FS,
                mount: "/mnt/p",
                serials: &self.serials,
                mount_opts: "noatime,degraded,compress=zstd",
                wait: Duration::from_secs(60),
                record_dir: self.dir.path(),
                intent,
            };
            (mount_guarded(host, &plan, &progress), progress)
        }
        fn primary(&self) -> Copy {
            read_copy(&primary_path(self.dir.path(), FS), FS)
        }
    }

    fn warnings(p: &TestProgress) -> Vec<String> {
        p.logs
            .lock()
            .unwrap()
            .iter()
            .filter(|(l, _)| matches!(l, LogLevel::Warning))
            .map(|(_, m)| m.clone())
            .collect()
    }

    fn both() -> FakeHost {
        FakeHost::new(vec![
            ("SERIAL11", "/dev/sdb1", 1, DX, 50, 0),
            ("SERIAL12", "/dev/sdc1", 2, DY, 50, 0),
        ])
    }

    fn only_x() -> FakeHost {
        FakeHost::new(vec![("SERIAL11", "/dev/sdb1", 1, DX, 60, 0)])
    }

    fn only_y() -> FakeHost {
        FakeHost::new(vec![("SERIAL12", "/dev/sdc1", 2, DY, 52, 0)])
    }

    fn refusal(r: Result<Outcome, Failure>) -> String {
        match r {
            Err(Failure::Refused { why, left_mounted }) => {
                assert!(!left_mounted);
                why
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn both_members_present_mount_without_degraded_and_scan_each_by_path() {
        let case = Case::new();
        let host = both();
        let (r, _) = case.run(&host);
        assert!(matches!(r, Ok(Outcome::Normal { owed: None, .. })), "{r:?}");
        assert_eq!(
            host.calls(),
            [
                format!("blkid {FS}"),
                "scan /dev/sdb1".into(),
                "scan /dev/sdc1".into(),
                format!("mount noatime,compress=zstd {FS} /mnt/p"),
            ]
        );
        assert_eq!(case.primary(), Copy::Absent, "nothing recorded");
    }

    #[test]
    fn a_late_member_inside_the_wait_costs_no_degraded_mount() {
        let case = Case::new();
        let host = FakeHost::new(vec![
            ("SERIAL11", "/dev/sdb1", 1, DX, 50, 0),
            ("SERIAL12", "/dev/sdc1", 2, DY, 50, 3),
        ]);
        let (r, _) = case.run(&host);
        match r {
            Ok(Outcome::Normal {
                waited, members, ..
            }) => {
                assert_eq!(waited, Duration::from_secs(15));
                assert_eq!(members.len(), 2);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            !host.calls().iter().any(|c| c.contains("degraded")),
            "{:?}",
            host.calls()
        );
        assert!(
            host.calls().contains(&"scan /dev/sdc1".to_string()),
            "rescanned"
        );
        assert_eq!(case.primary(), Copy::Absent);
    }

    #[test]
    fn an_absent_member_after_the_wait_mounts_degraded_records_both_copies_and_warns() {
        let case = Case::new();
        let host = only_x();
        let (r, progress) = case.run(&host);
        let record = match r {
            Ok(Outcome::Degraded {
                record,
                absent_serials,
                recorded,
                ..
            }) => {
                assert_eq!(absent_serials, ["SERIAL12"]);
                assert!(recorded);
                record
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(record.state, State::Degraded);
        assert_eq!(
            record.members,
            [PeriodMember {
                devid: 1,
                dev_uuid: DX.into(),
                generation: 60
            }]
        );
        assert_eq!(case.primary(), Copy::Read(record.clone()));
        assert_eq!(
            host.witness.borrow().get(DX),
            Some(&Copy::Read(record)),
            "the witness is on member X's view"
        );
        let calls = host.calls();
        let probe = calls
            .iter()
            .position(|c| c.starts_with(&format!("mount {PROBE_OPTS}")))
            .expect("probe mount");
        let rw = calls
            .iter()
            .position(|c| c == &format!("mount noatime,compress=zstd,degraded {FS} /mnt/p"))
            .expect("degraded mount");
        assert!(
            probe < rw && calls[probe + 1] == "umount /mnt/p",
            "{calls:?}"
        );
        assert_eq!(calls.last().unwrap(), "write witness");
        assert_eq!(
            calls.iter().filter(|c| c.starts_with("blkid")).count(),
            13,
            "looked again every 5 s for 60 s"
        );
        assert!(
            warnings(&progress)
                .iter()
                .any(|w| w.contains("DEGRADED") && w.contains("SERIAL12")),
            "{:?}",
            warnings(&progress)
        );
    }

    #[test]
    fn x_recorded_and_y_alone_is_refused_as_split_brain() {
        let case = Case::new();
        assert!(case.run(&only_x()).0.is_ok());
        let host = only_y();
        let why = refusal(case.run(&host).0);
        assert!(why.contains("split brain") && why.contains(DY), "{why}");
        assert!(
            !host
                .calls()
                .iter()
                .any(|c| c.starts_with("mount") && !c.contains(PROBE_OPTS)),
            "Y was mounted writable: {:?}",
            host.calls()
        );
    }

    /// Operator 2026-11-22 (bd DAS-Backup-Manager-su1b.11): a job that never
    /// writes the target may read the member that lacks the last degraded
    /// period's writes — read-only, nothing replayed, nothing recorded — and
    /// says the date its data stops.
    #[test]
    fn a_read_only_job_mounts_the_stale_member_read_only_and_records_nothing() {
        let case = Case::new();
        let period = match case.run(&only_x()).0 {
            Ok(Outcome::Degraded { record, .. }) => record,
            other => panic!("{other:?}"),
        };
        let before = case.primary();
        let host = only_y();
        let (r, progress) = case.run_as(&host, Intent::ReadOnly);
        match r {
            Ok(Outcome::StaleReadOnly {
                record, members, ..
            }) => {
                assert_eq!(record, period, "the record as it was");
                assert_eq!(members.len(), 1);
                assert_eq!(members[0].dev_uuid, DY);
            }
            other => panic!("{other:?}"),
        }
        let calls = host.calls();
        assert_eq!(
            calls.last().unwrap(),
            &format!("mount noatime,compress=zstd,ro,degraded,rescue=nologreplay {FS} /mnt/p"),
            "{calls:?}"
        );
        assert!(!calls.iter().any(|c| c == "write witness"), "{calls:?}");
        assert_eq!(case.primary(), before, "no record change");
        let when = crate::caldate::local_datetime(period.since).unwrap();
        assert!(
            warnings(&progress)
                .iter()
                .any(|w| w.contains("STALE-READ-ONLY") && w.contains(&when) && w.contains(DX)),
            "{:?}",
            warnings(&progress)
        );
        // A writing job is still refused the same member.
        let why = refusal(case.run_as(&only_y(), Intent::Write).0);
        assert!(why.contains("split brain"), "{why}");
    }

    #[test]
    fn a_read_only_job_gets_only_a_read_only_mount_when_the_record_cannot_be_read() {
        // Review F1 (binding): an unreadable record refuses every job that
        // writes; a read-only job gets the read-only mount, nothing written.
        let case = Case::new();
        fs::write(primary_path(case.dir.path(), FS), "garbage\n").unwrap();
        let host = only_y();
        match case.run_as(&host, Intent::ReadOnly).0 {
            Ok(Outcome::ReadOnly { why, .. }) => assert!(why.contains("unreadable"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(
            host.calls()
                .iter()
                .filter(|c| c.starts_with("mount"))
                .all(|c| c.contains(STALE_READ_OPTS)),
            "{:?}",
            host.calls()
        );
        assert!(!host.calls().iter().any(|c| c == "write witness"));
    }

    /// Only jobs that persist nothing derived from the target's contents may
    /// read a split-brain member: every job that writes the index, a record
    /// or a state file from what it reads is refused it.
    #[test]
    fn only_jobs_that_persist_nothing_from_the_target_read_a_split_brain_member() {
        let read_only: Vec<Job> = Job::ALL
            .into_iter()
            .filter(|j| j.intent() == Intent::ReadOnly)
            .collect();
        assert_eq!(
            read_only,
            [
                Job::RestoreFiles,
                Job::RestoreSnapshot,
                Job::Browse,
                Job::Doctor
            ]
        );
        for job in [
            Job::Walk,
            Job::IndexWalk,
            Job::Reindex,
            Job::Reconcile,
            Job::RecoveryOsRecord,
        ] {
            let case = Case::new();
            assert!(case.run(&only_x()).0.is_ok());
            let why = refusal(case.run_as(&only_y(), job.intent()).0);
            assert!(why.contains("split brain"), "{job:?}: {why}");
        }
    }

    /// Every caller names its job: each `ensure_targets_mounted(` and
    /// `mount_guarded_target(` call in the sources passes
    /// `Job::<job>.intent()`, never a bare `Intent`, so the table above is the
    /// whole classification.
    #[test]
    fn every_target_mount_names_its_job() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut paths = vec![src];
        let mut calls = 0;
        while let Some(p) = paths.pop() {
            if p.is_dir() {
                paths.extend(fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()));
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = fs::read_to_string(&p).unwrap();
            for name in ["ensure_targets_mounted(", "mount_guarded_target("] {
                for (at, _) in text.match_indices(name) {
                    let before = &text[..at];
                    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
                    let line_head = text[line_start..at].trim_start();
                    if before.ends_with("fn ")
                        || before.ends_with('"')
                        || line_head.starts_with("//")
                    {
                        continue;
                    }
                    let end = text[at..].find(';').map_or(text.len(), |e| at + e);
                    let args = &text[at..end];
                    assert!(
                        args.contains("Job::") && args.contains(".intent()"),
                        "{}: a target mount that does not name its job: {args}",
                        p.display()
                    );
                    calls += 1;
                }
            }
        }
        assert!(calls >= 15, "the scan found only {calls} calls");
    }

    /// The record `btrdasd raid accept` leaves: Y is the history from now,
    /// X is given up.
    fn accepted() -> Record {
        Record {
            fsid: FS.into(),
            state: State::Degraded,
            members: vec![PeriodMember {
                devid: 2,
                dev_uuid: DY.into(),
                generation: 52,
            }],
            since: 2000,
            last: 2000,
            rejoined: None,
            rejected: vec![PeriodMember {
                devid: 1,
                dev_uuid: DX.into(),
                generation: 60,
            }],
        }
    }

    fn put_primary(case: &Case, r: &Record) {
        write_copy(&primary_path(case.dir.path(), FS), r, true).unwrap();
    }

    /// bd DAS-Backup-Manager-su1b.11: a member `raid accept` gave up is
    /// refused alone, for every job, before it is registered with btrfs or
    /// anything is mounted.
    #[test]
    fn a_rejected_member_alone_is_refused_before_it_is_scanned_or_mounted() {
        for intent in [Intent::Write, Intent::ReadOnly] {
            let case = Case::new();
            put_primary(&case, &accepted());
            let host = only_x();
            let why = refusal(case.run_as(&host, intent).0);
            assert!(why.contains("raid accept") && why.contains(DX), "{why}");
            let calls = host.calls();
            assert!(
                !calls
                    .iter()
                    .any(|c| c.starts_with("scan") || c.starts_with("mount")),
                "{intent:?}: {calls:?}"
            );
            assert_eq!(case.primary(), Copy::Read(accepted()), "unchanged");
        }
    }

    /// Beside the accepted member: the plain mount would join two histories.
    /// Refused before X is scanned and before anything is mounted.
    #[test]
    fn a_rejected_member_beside_the_accepted_one_is_refused_before_any_mount() {
        let case = Case::new();
        put_primary(&case, &accepted());
        let host = both();
        let why = refusal(case.run(&host).0);
        assert!(why.contains("raid accept") && why.contains(DX), "{why}");
        let calls = host.calls();
        assert!(!calls.iter().any(|c| c == "scan /dev/sdb1"), "{calls:?}");
        assert!(!calls.iter().any(|c| c.starts_with("mount")), "{calls:?}");
    }

    /// The primary lost (a rollback of /): only the witness on the filesystem
    /// still names X. Both present mount normally before it can be read, so
    /// the mount is taken straight back down and refused.
    #[test]
    fn a_witness_naming_a_rejected_member_takes_the_normal_mount_back_down() {
        let case = Case::new();
        let host = both();
        host.witness
            .borrow_mut()
            .insert(format!("{DX}+{DY}"), Copy::Read(accepted()));
        let why = refusal(case.run(&host).0);
        assert!(why.contains("raid accept") && why.contains(DX), "{why}");
        assert_eq!(host.calls().last().unwrap(), "umount /mnt/p");
        assert!(!host.calls().iter().any(|c| c == "write witness"));
    }

    /// Y alone again after the accept continues Y's period, and the rejected
    /// member stays rejected.
    #[test]
    fn the_accepted_member_alone_continues_and_keeps_the_rejection() {
        let case = Case::new();
        put_primary(&case, &accepted());
        let host = only_y();
        host.now.set(3000);
        host.witness
            .borrow_mut()
            .insert(DY.into(), Copy::Read(accepted()));
        match case.run(&host).0 {
            Ok(Outcome::Degraded { record, .. }) => {
                assert_eq!(record.since, 2000, "the same period");
                assert!(record.last > 3000, "{}", record.last);
                assert_eq!(record.rejected, accepted().rejected);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A replacement joined (devid 1 again, a new device UUID): the scrub
    /// after it pays the debt, and the rejection is kept as `settled`, so the
    /// old drive, if it is ever attached unwiped, is still refused.
    #[test]
    fn the_rejection_outlives_the_scrub_as_settled() {
        const DZ: &str = "5eed00a3-0000-4000-8000-0000000000a3";
        let records = tempfile::TempDir::new().unwrap();
        let fs_root = tempfile::TempDir::new().unwrap();
        let mount = fs_root.path().to_str().unwrap();
        let owed = Record {
            state: State::ScrubOwed,
            rejoined: Some(4000),
            ..accepted()
        };
        write_copy(&primary_path(records.path(), FS), &owed, true).unwrap();
        write_copy(&witness_path(mount), &owed, false).unwrap();
        let cleared = clear_after_scrub(records.path(), mount, FS, 4001, Ok(vec![])).unwrap();
        assert_eq!(cleared, Some(owed.clone()));
        let settled = Record {
            state: State::Settled,
            rejoined: None,
            ..owed
        };
        assert_eq!(
            read_copy(&primary_path(records.path(), FS), FS),
            Copy::Read(settled.clone())
        );
        assert_eq!(
            read_copy(&witness_path(mount), FS),
            Copy::Read(settled.clone())
        );
        // Settled, every current member present: a normal mount, nothing owed.
        let case = Case::new();
        put_primary(&case, &settled);
        let host = FakeHost::new(vec![
            ("SERIAL13", "/dev/sdd1", 1, DZ, 70, 0),
            ("SERIAL12", "/dev/sdc1", 2, DY, 70, 0),
        ]);
        assert!(matches!(
            case.run(&host).0,
            Ok(Outcome::Normal {
                owed: None,
                record_problem: None,
                ..
            })
        ));
        // The old drive X beside the pair: refused.
        let host = FakeHost::new(vec![
            ("SERIAL13", "/dev/sdd1", 1, DZ, 70, 0),
            ("SERIAL12", "/dev/sdc1", 2, DY, 70, 0),
            ("SERIAL11", "/dev/sdb1", 1, DX, 60, 0),
        ]);
        assert!(refusal(case.run(&host).0).contains(DX));
    }

    #[test]
    fn a_rejection_round_trips_and_malformed_ones_are_bad() {
        let rec = accepted();
        let text = rec.render();
        let line = format!("rejected=1 {DX} 60\n");
        assert!(text.contains(&line), "{text}");
        assert_eq!(Record::parse(&text, FS), Ok(rec.clone()));
        let settled = Record {
            state: State::Settled,
            ..rec.clone()
        };
        assert_eq!(Record::parse(&settled.render(), FS), Ok(settled.clone()));
        for broken in [
            settled.render().replace(&line, ""),
            format!("{text}{line}"),
            text.replace(&line, &format!("rejected=2 {DY} 60\n")),
            text.replace(&line, "rejected=1 nope 60\n"),
        ] {
            assert!(Record::parse(&broken, FS).is_err(), "{broken}");
        }
        // A copy that lacks the rejection does not drop it.
        let plain = Record {
            rejected: Vec::new(),
            ..rec.clone()
        };
        let m = merged(&Copy::Read(plain), &Copy::Read(rec.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(m.rejected, rec.rejected);
    }

    fn plan_for<'a>(case: &'a Case) -> Plan<'a> {
        Plan {
            label: "primary",
            fsid: FS,
            mount: "/mnt/p",
            serials: &case.serials,
            mount_opts: "noatime,degraded,compress=zstd",
            wait: Duration::from_secs(60),
            record_dir: case.dir.path(),
            intent: Intent::Write,
        }
    }

    /// X recorded degraded (since 1060, last 1060), then Y alone: the split
    /// brain `raid accept` exists for.
    fn split_brain_case() -> (Case, Record) {
        let case = Case::new();
        let period = match case.run(&only_x()).0 {
            Ok(Outcome::Degraded { record, .. }) => record,
            other => panic!("{other:?}"),
        };
        (case, period)
    }

    fn accept_with(
        case: &Case,
        host: &FakeHost,
        typed: Result<&str, &str>,
    ) -> (Result<Accepted, String>, Vec<String>) {
        let mut shown = Vec::new();
        let mut confirm = |text: &str| {
            shown.push(text.to_string());
            typed.map(str::to_string).map_err(str::to_string)
        };
        let runs = |from: i64, to: i64| {
            assert!(from <= to);
            Ok(Some(3))
        };
        let r = accept(
            host,
            &plan_for(case),
            &runs,
            &mut confirm,
            &TestProgress::default(),
        );
        (r, shown)
    }

    #[test]
    fn accept_is_refused_unless_the_guard_refuses_the_target_for_split_brain() {
        // No record: a degraded mount would be allowed.
        let case = Case::new();
        let (r, shown) = accept_with(&case, &only_x(), Ok("primary"));
        assert!(r.unwrap_err().contains("not refused"));
        assert!(shown.is_empty(), "nothing asked");
        // Both present.
        let (case, _) = split_brain_case();
        let (r, _) = accept_with(&case, &both(), Ok("primary"));
        assert!(r.unwrap_err().contains("every member"));
        // The record unreadable: refused for another reason.
        let case = Case::new();
        fs::write(primary_path(case.dir.path(), FS), "junk\n").unwrap();
        let (r, _) = accept_with(&case, &only_y(), Ok("primary"));
        assert!(r.unwrap_err().contains("unreadable"));
        assert_eq!(
            read_copy(&primary_path(case.dir.path(), FS), FS),
            Copy::Bad(format!(
                "{}: the first line is not '{FORMAT_LINE}'",
                primary_path(case.dir.path(), FS).display()
            ))
        );
    }

    #[test]
    fn accept_needs_the_label_typed_exactly_and_writes_nothing_otherwise() {
        for typed in [
            Ok("primar"),
            Ok("primary "),
            Ok("PRIMARY"),
            Ok(""),
            Err("not a terminal"),
        ] {
            let (case, period) = split_brain_case();
            let host = only_y();
            let (r, shown) = accept_with(&case, &host, typed);
            assert!(r.is_err(), "{typed:?}");
            assert_eq!(shown.len(), 1, "explained once");
            assert_eq!(case.primary(), Copy::Read(period.clone()), "{typed:?}");
            let calls = host.calls();
            assert!(!calls.iter().any(|c| c == "write witness"), "{calls:?}");
            assert!(
                !calls
                    .iter()
                    .any(|c| c.starts_with("mount") && !c.contains(PROBE_OPTS)),
                "mounted writable: {calls:?}"
            );
        }
    }

    #[test]
    fn accept_makes_the_present_member_the_history_and_rejects_the_absent_one() {
        let (case, period) = split_brain_case();
        let host = only_y();
        host.now.set(7000);
        let (r, shown) = accept_with(&case, &host, Ok("primary"));
        let done = r.unwrap();
        // What was shown: Y by its superblock, the period on X, the runs only X holds.
        let text = &shown[0];
        for needle in [
            DY,
            "superblock generation 52",
            DX,
            "3 backup run",
            "SERIAL11",
        ] {
            assert!(text.contains(needle), "{needle}: {text}");
        }
        assert!(text.contains(&when(period.since)), "{text}");
        assert_eq!(done.before, period);
        let after = done.after.clone();
        assert_eq!(after.state, State::Degraded);
        assert_eq!(after.dev_uuids(), BTreeSet::from([DY]));
        assert_eq!(after.members[0].generation, 52);
        assert_eq!(after.rejected_uuids(), BTreeSet::from([DX]));
        assert_eq!(after.since, 7000);
        assert_eq!(case.primary(), Copy::Read(after.clone()));
        assert_eq!(
            host.witness.borrow().get(DY),
            Some(&Copy::Read(after.clone()))
        );
        let calls = host.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("mount"))
                .cloned()
                .collect::<Vec<_>>(),
            [
                format!("mount {PROBE_OPTS} {FS} /mnt/p"),
                format!("mount noatime,compress=zstd,degraded {FS} /mnt/p"),
            ]
        );
        assert_eq!(calls.last().unwrap(), "umount /mnt/p", "left unmounted");
        // From now on: Y alone is the period, X is refused alone and beside Y.
        assert!(matches!(
            case.run(&only_y()).0,
            Ok(Outcome::Degraded { .. })
        ));
        assert!(refusal(case.run(&only_x()).0).contains("raid accept"));
        assert!(refusal(case.run(&both()).0).contains("raid accept"));
        // And a second accept, X alone, is refused: X is rejected.
        let (r, _) = accept_with(&case, &only_x(), Ok("primary"));
        assert!(r.unwrap_err().contains("raid accept"));
    }

    #[test]
    fn accept_says_unknown_when_the_runs_cannot_be_counted() {
        let (case, _) = split_brain_case();
        let host = only_y();
        let found = split_brain_now(&host, &plan_for(&case), &TestProgress::default()).unwrap();
        let text = accept_explanation("primary", FS, &found, &Err("no database".into()));
        assert!(text.contains("unknown (no database)"), "{text}");
        assert!(!text.contains(" 0 backup run"), "{text}");
    }

    /// The primary copy was rewritten and the witness was not (the mount for
    /// it failed): a second accept finishes the first.
    #[test]
    fn a_half_finished_accept_is_finished_by_the_next() {
        let (case, period) = split_brain_case();
        let half = Record {
            members: vec![PeriodMember {
                devid: 2,
                dev_uuid: DY.into(),
                generation: 52,
            }],
            rejected: period.members.clone(),
            since: 6000,
            last: 6000,
            ..period.clone()
        };
        put_primary(&case, &half);
        let host = only_y();
        host.witness
            .borrow_mut()
            .insert(DY.into(), Copy::Read(period.clone()));
        let found = split_brain_now(&host, &plan_for(&case), &TestProgress::default()).unwrap();
        assert!(found.resumed);
        let (r, _) = accept_with(&case, &host, Ok("primary"));
        let after = r.unwrap().after;
        assert_eq!(
            host.witness.borrow().get(DY),
            Some(&Copy::Read(after.clone()))
        );
        assert_eq!(case.primary(), Copy::Read(after));
    }

    /// bd DAS-Backup-Manager-su1b.14: only the mount that turns a degraded
    /// record into scrub-owed is the rejoin; the next one owes the same scrub
    /// and is not.
    #[test]
    fn only_the_mount_that_rejoins_says_so() {
        let case = Case::new();
        assert!(case.run(&only_x()).0.is_ok());
        let first = case.run(&both()).0;
        assert!(
            matches!(
                first,
                Ok(Outcome::Normal {
                    rejoined_now: true,
                    owed: Some(_),
                    ..
                })
            ),
            "{first:?}"
        );
        let (line, code) = cli_line(&first);
        assert!(line.starts_with("RAID GUARD: REJOINED  ("), "{line}");
        assert_eq!(code, 0);
        let again = case.run(&both()).0;
        assert!(
            matches!(
                again,
                Ok(Outcome::Normal {
                    rejoined_now: false,
                    owed: Some(_),
                    ..
                })
            ),
            "{again:?}"
        );
        assert!(cli_line(&again).0.starts_with("RAID GUARD: SCRUB-OWED  ("));
    }

    /// A member that drops while a job has the target mounted (bd
    /// DAS-Backup-Manager-su1b.14): the end-of-job check records a degraded
    /// period on the members still present, so the next mount with both owes
    /// a scrub.
    #[test]
    fn a_member_that_drops_during_a_job_is_recorded_before_the_unmount() {
        let case = Case::new();
        let host = both();
        assert!(matches!(
            case.run(&host).0,
            Ok(Outcome::Normal { owed: None, .. })
        ));
        assert_eq!(
            check_before_unmount(&host, &plan_for(&case)),
            EndCheck::Whole
        );
        assert_eq!(case.primary(), Copy::Absent, "nothing to record");
        *host.missing.borrow_mut() = Ok(vec![2]);
        host.now.set(5000);
        let record = match check_before_unmount(&host, &plan_for(&case)) {
            EndCheck::Recorded { record, why } => {
                assert!(why.contains("devid(s) [2]"), "{why}");
                record
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(record.state, State::Degraded);
        assert_eq!(record.dev_uuids(), BTreeSet::from([DX]));
        assert_eq!((record.since, record.last), (5000, 5000));
        assert_eq!(case.primary(), Copy::Read(record.clone()));
        assert_eq!(
            host.witness.borrow().get(&format!("{DX}+{DY}")),
            Some(&Copy::Read(record.clone()))
        );
        // The next mount with both present is the rejoin.
        assert!(matches!(
            case.run(&both()).0,
            Ok(Outcome::Normal {
                rejoined_now: true,
                ..
            })
        ));
    }

    /// The kernel's view unreadable at the end: possibly dropped, so a period
    /// is recorded on every member found — the cautious answer, a scrub owed
    /// at the next mount.
    #[test]
    fn an_unreadable_view_at_the_end_records_a_period_on_every_member_found() {
        let case = Case::new();
        let host = both();
        assert!(case.run(&host).0.is_ok());
        *host.missing.borrow_mut() = Err("no such directory".into());
        match check_before_unmount(&host, &plan_for(&case)) {
            EndCheck::Recorded { record, why } => {
                assert!(why.contains("cannot be read"), "{why}");
                assert_eq!(record.dev_uuids(), BTreeSet::from([DX, DY]));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(case.primary(), Copy::Read(_)));
    }

    /// An unreadable record at the end: a member missing and nothing can be
    /// written over it — said, never a silent "recorded".
    #[test]
    fn a_drop_with_an_unreadable_record_is_a_problem_not_a_record() {
        let case = Case::new();
        let host = both();
        assert!(case.run(&host).0.is_ok());
        fs::write(primary_path(case.dir.path(), FS), "junk\n").unwrap();
        *host.missing.borrow_mut() = Ok(vec![1]);
        match check_before_unmount(&host, &plan_for(&case)) {
            EndCheck::Problem(why) => assert!(why.contains("unreadable"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// bd DAS-Backup-Manager-su1b.14: a rejoin noted in this process starts
    /// the owed scrub once the lock is let go — and a test build never starts
    /// the real unit.
    #[test]
    fn the_owed_scrub_is_started_after_release_and_never_by_a_test_build() {
        let label = "owed-scrub-test-target";
        note_owed_scrub(label);
        note_owed_scrub(label);
        let said = after_maintenance_released();
        // Another test's release may have taken it first; either way it is
        // gone and nothing real was started.
        if let Some(line) = said {
            assert!(line.contains("refused in a test build"), "{line}");
            assert!(line.starts_with("warning: "), "{line}");
        }
        assert!(!take_owed_scrub().iter().any(|l| l == label));
        assert!(
            start_owed_scrub()
                .unwrap_err()
                .contains("refused in a test build")
        );
    }

    /// Review F1 (bd DAS-Backup-Manager-su1b.11): the primary lost (a
    /// rollback of /), the rejected X attached beside the accepted Y. Their
    /// superblock generations differ and no readable record explains it:
    /// never a read-write mount (the kernel would take X's tree and the
    /// rejoin's scrub would "repair" Y from it). A read-only job gets a
    /// read-only mount.
    #[test]
    fn diverged_generations_with_no_record_are_refused_before_any_rw_mount() {
        let pair = || {
            let host = FakeHost::new(vec![
                ("SERIAL11", "/dev/sdb1", 1, DX, 60, 0),
                ("SERIAL12", "/dev/sdc1", 2, DY, 52, 0),
            ]);
            host.witness
                .borrow_mut()
                .insert(format!("{DX}+{DY}"), Copy::Read(accepted()));
            host
        };
        let case = Case::new();
        let host = pair();
        let why = refusal(case.run(&host).0);
        assert!(
            why.contains("generation") && why.contains("raid accept"),
            "{why}"
        );
        assert!(
            !host.calls().iter().any(|c| c.starts_with("mount ")),
            "{:?}",
            host.calls()
        );
        let host = pair();
        match case.run_as(&host, Intent::ReadOnly).0 {
            Ok(Outcome::ReadOnly { why, .. }) => assert!(why.contains("generation"), "{why}"),
            other => panic!("{other:?}"),
        }
        let mounts: Vec<String> = host
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("mount "))
            .collect();
        assert_eq!(
            mounts,
            [format!(
                "mount noatime,compress=zstd,{STALE_READ_OPTS} {FS} /mnt/p"
            )]
        );
        assert!(!host.calls().iter().any(|c| c == "write witness"));
        assert_eq!(case.primary(), Copy::Absent);
    }

    /// A healthy pair after a rollback of / (no primary, equal generations)
    /// mounts normally.
    #[test]
    fn equal_generations_with_no_record_mount_normally() {
        let case = Case::new();
        assert!(matches!(
            case.run(&both()).0,
            Ok(Outcome::Normal { owed: None, .. })
        ));
    }

    /// Review F1: an unreadable primary refuses every job that writes, before
    /// anything is scanned or mounted; a read-only job gets a read-only mount.
    #[test]
    fn an_unreadable_primary_refuses_every_write_mount() {
        let case = Case::new();
        fs::write(primary_path(case.dir.path(), FS), "junk\n").unwrap();
        let host = both();
        let why = refusal(case.run(&host).0);
        assert!(why.contains("unreadable"), "{why}");
        assert!(
            !host
                .calls()
                .iter()
                .any(|c| c.starts_with("mount") || c.starts_with("scan")),
            "{:?}",
            host.calls()
        );
        let host = both();
        assert!(matches!(
            case.run_as(&host, Intent::ReadOnly).0,
            Ok(Outcome::ReadOnly { .. })
        ));
        assert!(
            host.calls()
                .iter()
                .filter(|c| c.starts_with("mount"))
                .all(|c| c.contains(STALE_READ_OPTS)),
            "{:?}",
            host.calls()
        );
    }

    /// Review F2: the rejected X plugged in while the label is typed (udev
    /// registers it by itself): refused before anything is written or
    /// mounted read-write.
    #[test]
    fn accept_refuses_a_member_that_appears_during_the_prompt() {
        let (case, period) = split_brain_case();
        let host = FakeHost::new(vec![
            ("SERIAL12", "/dev/sdc1", 2, DY, 52, 0),
            ("SERIAL11", "/dev/sdb1", 1, DX, 60, 1),
        ]);
        let mut confirm = |_: &str| {
            host.polls.set(1);
            Ok("primary".to_string())
        };
        let runs = |_: i64, _: i64| Ok(Some(0));
        let why = accept(
            &host,
            &plan_for(&case),
            &runs,
            &mut confirm,
            &TestProgress::default(),
        )
        .unwrap_err();
        assert!(why.contains("unplug") && why.contains(DX), "{why}");
        assert_eq!(case.primary(), Copy::Read(period), "nothing written");
        let calls = host.calls();
        assert!(!calls.iter().any(|c| c == "write witness"), "{calls:?}");
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("mount") && !c.contains(PROBE_OPTS)),
            "{calls:?}"
        );
    }

    /// Review F2: the degraded mount came up with a member besides the
    /// accepted one: taken straight down, nothing written.
    #[test]
    fn accept_takes_down_a_mount_with_a_foreign_member_and_writes_nothing() {
        let (case, period) = split_brain_case();
        let host = only_y();
        *host.devinfo.borrow_mut() = Some(Ok(vec![(1, false), (2, false)]));
        let (r, _) = accept_with(&case, &host, Ok("primary"));
        let why = r.unwrap_err();
        assert!(why.contains("devid 1"), "{why}");
        assert_eq!(host.calls().last().unwrap(), "umount /mnt/p");
        assert!(!host.calls().iter().any(|c| c == "write witness"));
        assert_eq!(case.primary(), Copy::Read(period), "nothing written");
    }

    /// Review round 3 (F1-stale): a primary that reads but predates the
    /// accept (an old backup, a recovery OS's copy) — rejected nothing, the
    /// period on X — with X and Y attached and their generations apart. Each
    /// member's own witness is read alone: Y's rejects X. Refused before any
    /// read-write mount; every member forgotten for a probe is registered
    /// again.
    #[test]
    fn diverged_members_are_read_one_by_one_and_a_rejection_on_either_refuses() {
        let case = Case::new();
        let stale = Record {
            members: vec![PeriodMember {
                devid: 1,
                dev_uuid: DX.into(),
                generation: 60,
            }],
            rejected: Vec::new(),
            since: 500,
            last: 500,
            ..accepted()
        };
        put_primary(&case, &stale);
        let host = FakeHost::new(vec![
            ("SERIAL11", "/dev/sdb1", 1, DX, 60, 0),
            ("SERIAL12", "/dev/sdc1", 2, DY, 70, 0),
        ]);
        host.witness
            .borrow_mut()
            .insert(DX.into(), Copy::Read(stale.clone()));
        host.witness
            .borrow_mut()
            .insert(DY.into(), Copy::Read(accepted()));
        let why = refusal(case.run(&host).0);
        assert!(why.contains("raid accept") && why.contains(DX), "{why}");
        let calls = host.calls();
        assert!(
            !calls.iter().any(|c| c.starts_with("mount ")),
            "a filesystem mount: {calls:?}"
        );
        for (forgotten, probed) in [("/dev/sdc1", "/dev/sdb1"), ("/dev/sdb1", "/dev/sdc1")] {
            let f = calls
                .iter()
                .position(|c| *c == format!("forget {forgotten}"))
                .unwrap_or_else(|| panic!("{forgotten} not forgotten: {calls:?}"));
            let m = calls
                .iter()
                .position(|c| *c == format!("mount-dev {PROBE_OPTS} {probed} /run/fake-probe"))
                .unwrap_or_else(|| panic!("{probed} not probed alone: {calls:?}"));
            let r = calls
                .iter()
                .rposition(|c| *c == format!("scan {forgotten}"))
                .unwrap_or_else(|| panic!("{forgotten} not registered again: {calls:?}"));
            assert!(f < m && m < r, "{calls:?}");
        }
        assert!(
            !calls
                .iter()
                .any(|c| c == "forget" || c.ends_with("--forget"))
        );
        assert_eq!(case.primary(), Copy::Read(stale), "nothing written");
    }

    /// A member whose own probe mount fails: refused for writing, and the
    /// member forgotten for it is registered again all the same.
    #[test]
    fn a_failed_member_probe_refuses_and_still_registers_the_others_again() {
        let case = Case::new();
        let host = FakeHost::new(vec![
            ("SERIAL11", "/dev/sdb1", 1, DX, 60, 0),
            ("SERIAL12", "/dev/sdc1", 2, DY, 70, 0),
        ]);
        put_primary(
            &case,
            &Record {
                members: vec![PeriodMember {
                    devid: 1,
                    dev_uuid: DX.into(),
                    generation: 60,
                }],
                rejected: Vec::new(),
                ..accepted()
            },
        );
        host.fail_device_mount
            .borrow_mut()
            .insert("/dev/sdb1".into());
        let why = refusal(case.run(&host).0);
        assert!(why.contains("/dev/sdb1"), "{why}");
        let calls = host.calls();
        let f = calls
            .iter()
            .position(|c| c == "forget /dev/sdc1")
            .expect("forgot");
        let r = calls.iter().rposition(|c| c == "scan /dev/sdc1").unwrap();
        assert!(f < r, "{calls:?}");
        assert!(!calls.iter().any(|c| c.starts_with("mount ")), "{calls:?}");
    }

    /// The filesystem mounted somewhere else: nothing is forgotten or probed.
    #[test]
    fn diverged_members_of_a_filesystem_mounted_elsewhere_are_refused_untouched() {
        let case = Case::new();
        let host = FakeHost::new(vec![
            ("SERIAL11", "/dev/sdb1", 1, DX, 60, 0),
            ("SERIAL12", "/dev/sdc1", 2, DY, 70, 0),
        ]);
        host.mounted_elsewhere.set(true);
        let why = refusal(case.run(&host).0);
        assert!(why.contains("mounted"), "{why}");
        assert!(
            !host
                .calls()
                .iter()
                .any(|c| c.starts_with("forget") || c.starts_with("mount")),
            "{:?}",
            host.calls()
        );
    }

    /// A healthy pair (one generation): no forget, no probe.
    #[test]
    fn a_healthy_pair_is_never_forgotten_or_probed() {
        let case = Case::new();
        let host = both();
        assert!(matches!(case.run(&host).0, Ok(Outcome::Normal { .. })));
        assert!(
            !host
                .calls()
                .iter()
                .any(|c| c.starts_with("forget") || c.starts_with("mount-dev")),
            "{:?}",
            host.calls()
        );
    }

    /// Review round 3 (F2-gap): during the prompt a drive appears whose
    /// superblock cannot be read: refused, never filtered out.
    #[test]
    fn accept_refuses_a_listed_member_whose_superblock_cannot_be_read() {
        let (case, period) = split_brain_case();
        let host = FakeHost::new(vec![
            ("SERIAL12", "/dev/sdc1", 2, DY, 52, 0),
            ("SERIAL11", "/dev/sdb1", 1, DX, 60, 1),
        ]);
        host.bad_super.borrow_mut().insert("/dev/sdb1".into());
        let mut confirm = |_: &str| {
            host.polls.set(1);
            Ok("primary".to_string())
        };
        let runs = |_: i64, _: i64| Ok(Some(0));
        let why = accept(
            &host,
            &plan_for(&case),
            &runs,
            &mut confirm,
            &TestProgress::default(),
        )
        .unwrap_err();
        assert!(why.contains("/dev/sdb1"), "{why}");
        assert!(
            !host
                .calls()
                .iter()
                .any(|c| c.starts_with("mount ") && !c.contains(PROBE_OPTS)),
            "{:?}",
            host.calls()
        );
        assert_eq!(case.primary(), Copy::Read(period));
    }

    /// Review round 3 (F2-gap): the absent drive's serial is attached again
    /// during the prompt although blkid does not list it: refused.
    #[test]
    fn accept_refuses_when_the_absent_drive_s_serial_is_attached_again() {
        let (case, period) = split_brain_case();
        let host = FakeHost::new(vec![
            ("SERIAL12", "/dev/sdc1", 2, DY, 52, 0),
            ("SERIAL11", "/dev/sdb1", 1, DX, 60, 1),
        ]);
        let mut confirm = |_: &str| {
            host.polls.set(1);
            host.hide_members.set(true);
            Ok("primary".to_string())
        };
        let runs = |_: i64, _: i64| Ok(Some(0));
        let why = accept(
            &host,
            &plan_for(&case),
            &runs,
            &mut confirm,
            &TestProgress::default(),
        )
        .unwrap_err();
        assert!(why.contains("SERIAL11"), "{why}");
        assert!(
            !host
                .calls()
                .iter()
                .any(|c| c.starts_with("mount ") && !c.contains(PROBE_OPTS)),
            "{:?}",
            host.calls()
        );
        assert_eq!(case.primary(), Copy::Read(period));
    }

    #[test]
    fn x_alone_again_continues_the_same_period() {
        let case = Case::new();
        let first = match case.run(&only_x()).0 {
            Ok(Outcome::Degraded { record, .. }) => record,
            other => panic!("{other:?}"),
        };
        let host = only_x();
        host.now.set(5000);
        // Its own view carries the witness written in the first period.
        host.witness
            .borrow_mut()
            .insert(DX.into(), Copy::Read(first.clone()));
        match case.run(&host).0 {
            Ok(Outcome::Degraded { record, .. }) => {
                assert_eq!(record.since, first.since, "same period");
                assert!(record.last > first.last);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn both_present_after_a_degraded_period_mount_normally_and_owe_a_scrub() {
        let case = Case::new();
        assert!(case.run(&only_x()).0.is_ok());
        let host = both();
        host.now.set(9000);
        let (r, progress) = case.run(&host);
        let owed = match r {
            Ok(Outcome::Normal {
                owed: Some(r),
                record_problem: None,
                ..
            }) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(owed.state, State::ScrubOwed);
        assert_eq!(owed.rejoined, Some(9000));
        assert_eq!(case.primary(), Copy::Read(owed.clone()));
        assert_eq!(
            host.witness.borrow().get(&format!("{DX}+{DY}")),
            Some(&Copy::Read(owed.clone()))
        );
        assert!(
            warnings(&progress)
                .iter()
                .any(|w| w.contains("REJOINED") && w.contains("a scrub is owed"))
        );
        assert!(!scrub_clears(&owed, 8999) && !scrub_clears(&owed, 9000));
        assert!(scrub_clears(&owed, 9001));
    }

    #[test]
    fn y_alone_while_the_scrub_is_still_owed_is_refused() {
        let case = Case::new();
        assert!(case.run(&only_x()).0.is_ok());
        assert!(case.run(&both()).0.is_ok());
        let why = refusal(case.run(&only_y()).0);
        assert!(
            why.contains("split brain") && why.contains("scrub owed"),
            "{why}"
        );
    }

    #[test]
    fn an_unreadable_record_refuses_a_degraded_mount() {
        let case = Case::new();
        fs::write(primary_path(case.dir.path(), FS), "garbage\n").unwrap();
        let host = only_x();
        let why = refusal(case.run(&host).0);
        assert!(why.contains("unreadable"), "{why}");
        assert!(
            !host
                .calls()
                .iter()
                .any(|c| c.contains("compress=zstd,degraded"))
        );
    }

    #[test]
    fn a_corrupt_witness_refuses_a_degraded_mount() {
        let case = Case::new();
        let host = only_x();
        host.witness
            .borrow_mut()
            .insert(DX.into(), Copy::Bad("bad bytes".into()));
        let why = refusal(case.run(&host).0);
        assert!(why.contains("witness"), "{why}");
        assert_eq!(case.primary(), Copy::Absent, "nothing recorded");
    }

    #[test]
    fn a_witness_alone_still_refuses_a_split_brain() {
        // The primary lost (a rollback of /); the witness Y's view holds is the
        // scrub-owed record written while both were present.
        let case = Case::new();
        assert!(case.run(&only_x()).0.is_ok());
        assert!(case.run(&both()).0.is_ok());
        let owed = host_witness(&case);
        fs::remove_file(primary_path(case.dir.path(), FS)).unwrap();
        let host = only_y();
        host.witness
            .borrow_mut()
            .insert(DY.into(), Copy::Read(owed));
        let why = refusal(case.run(&host).0);
        assert!(why.contains("split brain"), "{why}");
    }

    fn host_witness(case: &Case) -> Record {
        match case.primary() {
            Copy::Read(r) => r,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_drive_attached_but_the_kernel_shows_a_member_missing_fails() {
        // The 2026-11-21 shape: both drives attached, the mount succeeded,
        // and the kernel reports devid 2 missing.
        let case = Case::new();
        let host = both();
        *host.missing.borrow_mut() = Ok(vec![2]);
        let why = refusal(case.run(&host).0);
        assert!(why.contains("missing"), "{why}");
        assert_eq!(host.calls().last().unwrap(), "umount /mnt/p");
    }

    #[test]
    fn an_unreadable_kernel_view_after_mounting_fails() {
        let case = Case::new();
        let host = both();
        *host.missing.borrow_mut() = Err("no such directory".into());
        assert!(refusal(case.run(&host).0).contains("cannot be read"));
    }

    #[test]
    fn every_drive_attached_but_a_member_not_found_is_refused_never_degraded() {
        let case = Case::new();
        let host = both();
        host.hide_members.set(true);
        let why = refusal(case.run(&host).0);
        assert!(why.contains("every configured drive"), "{why}");
        assert!(!host.calls().iter().any(|c| c.starts_with("mount")));
    }

    #[test]
    fn a_failing_plain_mount_with_every_member_present_never_goes_degraded() {
        let case = Case::new();
        let host = both();
        host.fail_mount_plain.set(true);
        let why = refusal(case.run(&host).0);
        assert!(why.contains("not mounting it degraded"), "{why}");
        assert!(!host.calls().iter().any(|c| c.contains("degraded")));
    }

    /// The incident's kernel (bd CachyOS-Kernel session, VM-proven): after a
    /// `--forget`, with one leg scanned, the plain mount fails (rc 32) though
    /// both legs are attached. Each poll scans every member again; once the
    /// second leg registers, the plain mount succeeds — no degraded mount.
    #[test]
    fn a_member_the_kernel_forgot_is_rescanned_until_the_plain_mount_succeeds() {
        let case = Case::new();
        let host = both();
        host.model_kernel.set(true);
        host.scan_failures.set(2);
        let (r, _) = case.run(&host);
        match r {
            Ok(Outcome::Normal { waited, .. }) => assert_eq!(waited, Duration::from_secs(10)),
            other => panic!("{other:?}"),
        }
        let calls = host.calls();
        assert_eq!(
            calls.iter().filter(|c| *c == "scan /dev/sdc1").count(),
            3,
            "{calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.starts_with("mount noatime,compress=zstd "))
                .count(),
            3,
            "the plain mount, tried again after each rescan: {calls:?}"
        );
        assert!(!calls.iter().any(|c| c.contains("degraded")), "{calls:?}");
        assert_eq!(case.primary(), Copy::Absent);
    }

    /// The same kernel, and the leg never registers: refused after the wait,
    /// never degraded, with every configured drive attached.
    #[test]
    fn a_member_the_kernel_never_takes_back_is_refused_never_degraded() {
        let case = Case::new();
        let host = both();
        host.model_kernel.set(true);
        host.scan_failures.set(u32::MAX);
        let why = refusal(case.run(&host).0);
        assert!(why.contains("not mounting it degraded"), "{why}");
        let calls = host.calls();
        assert_eq!(
            calls.iter().filter(|c| *c == "scan /dev/sdc1").count(),
            13,
            "rescanned every 5 s"
        );
        assert!(!calls.iter().any(|c| c.contains("degraded")), "{calls:?}");
        assert_eq!(case.primary(), Copy::Absent, "nothing recorded");
    }

    #[test]
    fn no_member_found_is_not_available() {
        let case = Case::new();
        let host = FakeHost::new(vec![]);
        assert!(matches!(case.run(&host).0, Err(Failure::NoMember(_))));
    }

    #[test]
    fn a_read_only_degraded_mount_writes_no_record() {
        let case = Case::new();
        let host = only_x();
        let progress = TestProgress::default();
        let plan = Plan {
            label: "primary",
            fsid: FS,
            mount: "/mnt/p",
            serials: &case.serials,
            mount_opts: "noatime,ro",
            wait: Duration::ZERO,
            record_dir: case.dir.path(),
            intent: Intent::ReadOnly,
        };
        match mount_guarded(&host, &plan, &progress) {
            Ok(Outcome::Degraded {
                recorded: false, ..
            }) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(case.primary(), Copy::Absent);
        assert!(host.witness.borrow().is_empty());
    }

    #[test]
    fn the_cli_line_names_the_status_and_exits_by_it() {
        let case = Case::new();
        let (ok, code) = cli_line(&case.run(&both()).0);
        assert!(
            ok.starts_with("RAID GUARD: OK  (all 2 members present"),
            "{ok}"
        );
        assert_eq!(code, 0);
        let (deg, code) = cli_line(&case.run(&only_x()).0);
        assert!(
            deg.starts_with("RAID GUARD: DEGRADED  (RAID-1 MOUNTED DEGRADED"),
            "{deg}"
        );
        assert_eq!(code, 0);
        let (refused, code) = cli_line(&case.run(&only_y()).0);
        assert!(
            refused.starts_with("RAID GUARD: REFUSED  (split brain"),
            "{refused}"
        );
        assert_eq!(code, 3);
        let (none, code) = cli_line(&case.run(&FakeHost::new(vec![])).0);
        assert!(none.starts_with("RAID GUARD: NOT-MOUNTED  ("), "{none}");
        assert_eq!(code, 1);
        let (left, _) = cli_line(&Err(Failure::Refused {
            why: "a\nb".into(),
            left_mounted: true,
        }));
        assert_eq!(left, "RAID GUARD: REFUSED  (a b (still mounted))");
    }

    #[test]
    fn only_a_scrub_begun_after_the_rejoin_clears_the_record() {
        let records = tempfile::TempDir::new().unwrap();
        let fs_root = tempfile::TempDir::new().unwrap();
        let mount = fs_root.path().to_str().unwrap();
        let owed = Record {
            fsid: FS.into(),
            state: State::ScrubOwed,
            members: vec![PeriodMember {
                devid: 1,
                dev_uuid: DX.into(),
                generation: 9,
            }],
            since: 100,
            last: 200,
            rejoined: Some(300),
            rejected: Vec::new(),
        };
        let primary = primary_path(records.path(), FS);
        let put = |r: &Record| {
            write_copy(&primary, r, true).unwrap();
            write_copy(&witness_path(mount), r, false).unwrap();
        };
        put(&owed);
        let clear =
            |started, missing| clear_after_scrub(records.path(), mount, FS, started, missing);
        assert_eq!(
            clear(300, Ok(vec![])),
            Ok(None),
            "begun at the rejoin, not after"
        );
        assert!(clear(301, Ok(vec![2])).is_err(), "a member missing");
        assert!(clear(301, Err("no sysfs".into())).is_err());
        assert_eq!(
            read_copy(&primary, FS),
            Copy::Read(owed.clone()),
            "kept on doubt"
        );
        assert_eq!(clear(301, Ok(vec![])), Ok(Some(owed.clone())));
        assert_eq!(read_copy(&primary, FS), Copy::Absent);
        assert_eq!(read_copy(&witness_path(mount), FS), Copy::Absent);
        assert_eq!(clear(301, Ok(vec![])), Ok(None), "nothing left to clear");
        // Still degraded: a scrub never clears that.
        put(&Record {
            state: State::Degraded,
            rejoined: None,
            ..owed.clone()
        });
        assert_eq!(clear(9999, Ok(vec![])), Ok(None));
        // An unreadable copy: nothing cleared.
        fs::write(witness_path(mount), "junk").unwrap();
        assert!(clear(9999, Ok(vec![])).is_err());
        assert!(matches!(read_copy(&primary, FS), Copy::Read(_)));
    }

    #[test]
    fn dump_super_keys_are_matched_exactly_and_once() {
        let m = parse_super("/dev/sdb1", &sup(1, DX, 77), FS).unwrap();
        assert_eq!((m.devid, m.generation, m.num_devices), (1, 77, 2));
        assert_eq!(m.dev_uuid, DX);
        // Another filesystem's member.
        assert!(parse_super("/dev/sdb1", &sup(1, DX, 77), DY).is_err());
        // A key twice, a key missing, nothing at all.
        let twice = format!("{}generation\t\t8\n", sup(1, DX, 77));
        assert!(parse_super("p", &twice, FS).unwrap_err().contains("twice"));
        let no_uuid: String = sup(1, DX, 77)
            .lines()
            .filter(|l| !l.starts_with("dev_item.uuid"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(
            parse_super("p", &no_uuid, FS)
                .unwrap_err()
                .contains("dev_item.uuid")
        );
        assert!(parse_super("p", "", FS).is_err());
    }

    #[test]
    fn a_record_round_trips_and_anything_malformed_is_bad() {
        let rec = Record {
            fsid: FS.into(),
            state: State::ScrubOwed,
            members: vec![PeriodMember {
                devid: 1,
                dev_uuid: DX.into(),
                generation: 9,
            }],
            since: 1,
            last: 2,
            rejoined: Some(3),
            rejected: Vec::new(),
        };
        assert_eq!(Record::parse(&rec.render(), FS), Ok(rec.clone()));
        assert!(
            Record::parse(&rec.render(), DY).is_err(),
            "another filesystem's"
        );
        let text = rec.render();
        for broken in [
            text.replacen(FORMAT_LINE, "format=2", 1),
            format!("{text}extra=1\n"),
            format!("{text}last=4\n"),
            text.replace("rejoined=3\n", ""),
            text.replace("state=scrub-owed", "state=degraded"),
            text.replace(&format!("member=1 {DX} 9\n"), ""),
            text.replace(DX, "not-a-uuid"),
        ] {
            assert!(Record::parse(&broken, FS).is_err(), "{broken}");
        }
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("sub").join("r.record");
        assert_eq!(read_copy(&p, FS), Copy::Absent);
        write_copy(&p, &rec, true).unwrap();
        assert_eq!(read_copy(&p, FS), Copy::Read(rec));
        fs::create_dir(dir.path().join("d")).unwrap();
        assert!(matches!(read_copy(&dir.path().join("d"), FS), Copy::Bad(_)));
        remove_copy(&p).unwrap();
        remove_copy(&p).unwrap();
        assert_eq!(read_copy(&p, FS), Copy::Absent);
    }

    #[test]
    fn copies_that_disagree_on_the_members_refuse() {
        let a = Record {
            fsid: FS.into(),
            state: State::Degraded,
            members: vec![PeriodMember {
                devid: 1,
                dev_uuid: DX.into(),
                generation: 1,
            }],
            since: 1,
            last: 1,
            rejoined: None,
            rejected: Vec::new(),
        };
        let b = Record {
            members: vec![PeriodMember {
                devid: 2,
                dev_uuid: DY.into(),
                generation: 1,
            }],
            ..a.clone()
        };
        assert!(merged(&Copy::Read(a.clone()), &Copy::Read(b)).is_err());
        assert_eq!(merged(&Copy::Absent, &Copy::Read(a.clone())), Ok(Some(a)));
        assert_eq!(merged(&Copy::Absent, &Copy::Absent), Ok(None));
    }

    #[test]
    fn degraded_is_stripped_and_added_by_exact_token() {
        assert_eq!(
            without_degraded("noatime, degraded ,degraded_x,degraded"),
            "noatime,degraded_x"
        );
        assert_eq!(with_degraded("degraded"), "degraded");
        assert_eq!(
            with_degraded("noatime,degraded,compress"),
            "noatime,compress,degraded"
        );
        assert!(is_read_only("noatime,ro") && !is_read_only("noatime,rodata"));
    }

    #[test]
    fn the_kernel_view_is_read_from_devinfo_and_fails_closed() {
        let dir = tempfile::TempDir::new().unwrap();
        let devinfo = dir.path().join(FS).join("devinfo");
        assert!(missing_devids_under(dir.path(), FS).is_err(), "not mounted");
        for (id, flag) in [("1", "0\n"), ("2", "1\n")] {
            fs::create_dir_all(devinfo.join(id)).unwrap();
            fs::write(devinfo.join(id).join("missing"), flag).unwrap();
        }
        assert_eq!(missing_devids_under(dir.path(), FS), Ok(vec![2]));
        fs::write(devinfo.join("2").join("missing"), "0\n").unwrap();
        assert_eq!(missing_devids_under(dir.path(), FS), Ok(vec![]));
        fs::write(devinfo.join("2").join("missing"), "?\n").unwrap();
        assert!(missing_devids_under(dir.path(), FS).is_err());
        fs::remove_file(devinfo.join("2").join("missing")).unwrap();
        assert!(
            missing_devids_under(dir.path(), FS).is_err(),
            "an unreadable flag"
        );
    }

    #[test]
    fn a_target_is_guarded_only_with_a_uuid_and_two_serials() {
        let mut t = crate::config::Target {
            label: "p".into(),
            serial: "A".into(),
            serials: vec!["A".into(), "B".into()],
            mount_uuid: Some(FS.into()),
            mount: "/mnt/p".into(),
            role: crate::config::TargetRole::Primary,
            retention: crate::config::Retention {
                daily: 1,
                weekly: 0,
                monthly: 0,
                yearly: 0,
            },
            display_name: "p".into(),
            esp_partuuid: None,
        };
        assert!(is_guarded(&t));
        t.serials = vec!["A".into()];
        assert!(!is_guarded(&t));
        t.serials = vec!["A".into(), "B".into()];
        t.mount_uuid = None;
        assert!(!is_guarded(&t));
    }
}
