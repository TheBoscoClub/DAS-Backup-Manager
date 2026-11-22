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
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Degraded => "degraded",
            Self::ScrubOwed => "scrub-owed",
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
}

impl Record {
    /// The per-device UUIDs the period ran on.
    pub fn dev_uuids(&self) -> BTreeSet<&str> {
        self.members.iter().map(|m| m.dev_uuid.as_str()).collect()
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
        out.push_str(&format!("since={}\nlast={}\n", self.since, self.last));
        if let Some(r) = self.rejoined {
            out.push_str(&format!("rejoined={r}\n"));
        }
        out
    }

    /// Parse one copy, strictly: the format line first, every key known,
    /// each once (`member` one or more times), `rejoined` exactly when the
    /// state is `scrub-owed`, and `fsid` equal to `fsid`.
    pub fn parse(text: &str, fsid: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        if lines.next() != Some(FORMAT_LINE) {
            return Err(format!("the first line is not '{FORMAT_LINE}'"));
        }
        let (mut rec_fsid, mut state, mut since, mut last, mut rejoined) =
            (None, None, None, None, None);
        let mut members = Vec::new();
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
                "member" => {
                    let parts: Vec<&str> = value.split(' ').collect();
                    let [devid, dev_uuid, generation] = parts[..] else {
                        return Err(format!(
                            "member '{value}' is not '<devid> <dev_uuid> <generation>'"
                        ));
                    };
                    let (Ok(devid), Ok(generation)) = (devid.parse(), generation.parse()) else {
                        return Err(format!(
                            "member '{value}' has a devid or generation that is not a number"
                        ));
                    };
                    if !is_uuid(dev_uuid) {
                        return Err(format!("member '{value}' has no device UUID"));
                    }
                    members.push(PeriodMember {
                        devid,
                        dev_uuid: dev_uuid.to_ascii_lowercase(),
                        generation,
                    });
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
            other => {
                return Err(format!(
                    "state '{other}' is neither degraded nor scrub-owed"
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
            _ => {}
        }
        if members.is_empty() {
            return Err("no 'member' line".into());
        }
        Ok(Self {
            fsid: rec_fsid.to_ascii_lowercase(),
            state,
            members,
            since,
            last,
            rejoined,
        })
    }
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
            let degraded = a.state == State::Degraded || b.state == State::Degraded;
            Ok(Some(Record {
                fsid: a.fsid.clone(),
                state: if degraded {
                    State::Degraded
                } else {
                    State::ScrubOwed
                },
                members: a.members.clone(),
                since: a.since.min(b.since),
                last: a.last.max(b.last),
                rejoined: if degraded {
                    None
                } else {
                    a.rejoined.max(b.rejoined)
                },
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
        });
    };
    let period = record.dev_uuids();
    let strangers: Vec<String> = present
        .iter()
        .filter(|m| !period.contains(m.dev_uuid.as_str()))
        .map(|m| format!("devid {} ({})", m.devid, m.dev_uuid))
        .collect();
    if !strangers.is_empty() {
        return Verdict::Refuse(format!(
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
        ));
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
    let record = match record.state {
        State::Degraded => Record {
            state: State::ScrubOwed,
            rejoined: Some(now),
            ..record
        },
        State::ScrubOwed => record,
    };
    Rejoin::Owed {
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
    remove_copy(&witness_path)?;
    remove_copy(&primary_path)?;
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
    /// `btrfs inspect-internal dump-super <path>`.
    fn read_super(&self, path: &str) -> Result<String, String>;
    /// Whether a drive with this serial is attached.
    fn serial_attached(&self, serial: &str) -> bool;
    /// `mount -t btrfs [-o opts] UUID=<fsid> <mount>`.
    fn mount(&self, opts: &str, fsid: &str, mount: &str) -> Result<(), String>;
    fn umount(&self, mount: &str) -> Result<(), String>;
    /// The kernel's missing devids for the mounted filesystem.
    fn missing_devids(&self, fsid: &str) -> Result<Vec<u64>, String>;
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
    },
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
    /// The one-line status this mount reports: `OK`, `SCRUB-OWED` or
    /// `DEGRADED`, and the detail.
    pub fn status_line(&self) -> (&'static str, String) {
        match self {
            Self::Normal {
                members,
                waited,
                owed,
                record_problem,
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
                        "SCRUB-OWED",
                        format!(
                            "{base}; a scrub is owed before the pair is trusted: degraded on {} \
                             from {} to {}, rejoined {}",
                            names(&r.dev_uuids()),
                            r.since,
                            r.last,
                            r.rejoined.unwrap_or(r.last)
                        ),
                    )
                } else {
                    ("OK", base)
                }
            }
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
/// STATUS one of `OK`, `WARN`, `SCRUB-OWED`, `DEGRADED` (exit 0: mounted),
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
}

impl Survey {
    fn all_present(&self) -> bool {
        !self.members.is_empty() && self.members.len() as u64 >= self.expected
    }
}

fn survey(host: &dyn MemberHost, plan: &Plan<'_>, progress: &dyn ProgressCallback) -> Survey {
    let warn =
        |m: String| progress.on_log(LogLevel::Warning, &format!("Target '{}': {m}", plan.label));
    let paths = host.members(plan.fsid).unwrap_or_else(|e| {
        warn(format!("cannot list the members of {}: {e}", plan.fsid));
        Vec::new()
    });
    let mut members: Vec<Member> = Vec::new();
    for path in paths {
        if let Err(e) = host.scan(&path) {
            warn(format!("btrfs device scan {path} failed: {e}"));
        }
        let member = host
            .read_super(&path)
            .and_then(|text| parse_super(&path, &text, plan.fsid));
        match member {
            Ok(m) if members.iter().any(|o| o.devid == m.devid) => {
                warn(format!("{path} is devid {} again — counted once", m.devid))
            }
            Ok(m) => members.push(m),
            Err(e) => warn(format!("not counted as a member: {e}")),
        }
    }
    let expected = members.iter().map(|m| m.num_devices).max().unwrap_or(0);
    Survey { members, expected }
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
    let found = loop {
        let s = survey(host, plan, progress);
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
    let (owed, record_problem) = match rejoin(&primary, &witness, host.now()) {
        Rejoin::Clean => (None, None),
        Rejoin::Unreadable(e) => (None, Some(format!("{e}; nothing changed"))),
        Rejoin::Owed {
            record,
            write_primary,
            write_witness,
        } => {
            let mut problems = Vec::new();
            if !is_read_only(plan.mount_opts) {
                if write_primary && let Err(e) = write_copy(&primary_path, &record, true) {
                    problems.push(e);
                }
                if write_witness && let Err(e) = host.write_witness(plan.mount, &record) {
                    problems.push(e);
                }
            }
            (
                Some(record),
                (!problems.is_empty()).then(|| problems.join("; ")),
            )
        }
    };
    let outcome = Outcome::Normal {
        members: s.members,
        waited,
        owed,
        record_problem,
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

    impl FakeHost {
        fn new(drives: Vec<(&'static str, &'static str, u64, &'static str, u64, u32)>) -> Self {
            Self {
                drives,
                polls: Cell::new(0),
                calls: RefCell::new(Vec::new()),
                mounted: RefCell::new(None),
                witness: RefCell::new(BTreeMap::new()),
                missing: RefCell::new(Ok(Vec::new())),
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
        fn read_super(&self, path: &str) -> Result<String, String> {
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
            Ok(())
        }
        fn missing_devids(&self, _fsid: &str) -> Result<Vec<u64>, String> {
            self.missing.borrow().clone()
        }
        fn read_witness(&self, _mount: &str, _fsid: &str) -> Copy {
            assert!(
                self.mounted.borrow().is_some(),
                "witness read while unmounted"
            );
            self.witness
                .borrow()
                .get(&self.view())
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
            let progress = TestProgress::default();
            let plan = Plan {
                label: "primary",
                fsid: FS,
                mount: "/mnt/p",
                serials: &self.serials,
                mount_opts: "noatime,degraded,compress=zstd",
                wait: Duration::from_secs(60),
                record_dir: self.dir.path(),
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
        assert!(warnings(&progress).iter().any(|w| w.contains("SCRUB-OWED")));
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
