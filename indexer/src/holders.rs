//! Who holds a target's filesystem, and clearing them (bd
//! DAS-Backup-Manager-su1b.12, operator 2026-11-23).
//!
//! A target filesystem another program mounted (udisks under `/run/media`, a
//! systemd `.mount` unit, a hand mount), or a mount of it an earlier job left
//! busy at the target's own path, must come down before this project mounts
//! it its own way. This module finds every mount of the filesystem
//! ([`parse_mountinfo`], [`mounts_of`]), says where each came from
//! ([`Origin`]), finds every process using one ([`scan_holders`]: open files
//! with their open modes, mappings, cwd, root, executable), and applies the
//! operator's stop policy ([`clear_holders`]):
//!
//! - a process holding a file there OPEN FOR WRITING (fdinfo `flags` with
//!   `O_WRONLY`/`O_RDWR`, or a shared writable mapping), or one whose use
//!   cannot be read, is never signalled: the target is refused and the
//!   holders named;
//! - PID 1, this process, its ancestors and kernel threads are never
//!   signalled: one of them holding it refuses;
//! - a job that writes nothing under the target (verify, walk, index,
//!   restore, browse, doctor: `raid_guard::Job::stops_readers`) never
//!   signals anything: any holder refuses it, named (operator 2026-11-25);
//! - otherwise every holder gets SIGTERM, [`TERM_GRACE`] to go, then SIGKILL
//!   and [`KILL_WAIT`]; each signal is named in the returned lines;
//! - before each signal the pid's start time is read again, and a pid whose
//!   start time changed (reused) is skipped; before SIGKILL each holder is
//!   classified again, and one that became a writer refuses.
//!
//! Everything reads a `/proc` given as a path, so the tests build one in a
//! temporary directory; the signals go through [`Signaller`], whose
//! production implementation refuses in every test build.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long holders have to exit after SIGTERM.
pub const TERM_GRACE: Duration = Duration::from_secs(10);
/// How long the kernel has to reap them after SIGKILL.
pub const KILL_WAIT: Duration = Duration::from_secs(5);
/// The interval at which holders are looked for again while waiting.
pub const POLL: Duration = Duration::from_secs(1);

/// `PF_KTHREAD` in `/proc/<pid>/stat`'s flags field.
const PF_KTHREAD: u64 = 0x0020_0000;

// ---------------------------------------------------------------------------
// Mounts
// ---------------------------------------------------------------------------

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// The mount id (field 1): what `fdinfo`'s `mnt_id` names.
    pub id: u64,
    /// The directory of the filesystem shown there (field 4).
    pub root: String,
    /// `major:minor` of the mounted filesystem's device (field 3): the
    /// block device's own number for a filesystem on a block device (not
    /// btrfs, whose number is an anonymous one), what a FUSE mount can
    /// never carry. The source string is not evidence: any user can make a
    /// FUSE mount whose source reads `/dev/sdX1`.
    pub dev: String,
    pub mount_point: String,
    /// Per-mount options (field 6).
    pub options: String,
    pub fstype: String,
    pub source: String,
    /// The filesystem-wide options (the last field).
    pub super_options: String,
}

/// `\040`, `\011`, `\012`, `\134`: how mountinfo escapes a path.
fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every well-formed line of mountinfo text; a malformed line is skipped.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((mine, theirs)) = line.split_once(" - ") else {
            continue;
        };
        let left: Vec<&str> = mine.split(' ').collect();
        let right: Vec<&str> = theirs.split(' ').collect();
        if left.len() < 6 || right.len() < 3 {
            continue;
        }
        let Ok(id) = left[0].parse::<u64>() else {
            continue;
        };
        out.push(MountEntry {
            id,
            root: unescape(left[3]),
            dev: left[2].to_string(),
            mount_point: unescape(left[4]),
            options: left[5].to_string(),
            fstype: right[0].to_string(),
            source: unescape(right[1]),
            super_options: right[2..].join(" "),
        });
    }
    out
}

/// The mounts of one btrfs filesystem: those whose source is one of its
/// member devices. Mountinfo carries no filesystem UUID, and a multi-device
/// btrfs is listed under whichever member the kernel picked, so every
/// member's path is tried, as given and canonicalized. Deepest mount point
/// first: the order they come down in.
pub fn mounts_of(entries: &[MountEntry], member_paths: &[String]) -> Vec<MountEntry> {
    mounts_matching(entries, member_paths, |e| e.fstype == "btrfs")
}

/// A partition (or whole disk) as the host knows it: its path and its
/// block device number, `major:minor` (`st_rdev`, read by the caller from
/// the path, which must be a block device).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionId {
    pub path: String,
    pub dev: String,
}

/// The mounts of these partitions, of any filesystem (bd
/// DAS-Backup-Manager-eg4j.19): the recovery drive's vfat ESP that udisks
/// mounted is as much its mount as a btrfs one. Never by the source text:
/// a FUSE mount can say any source it likes. A btrfs mount is matched as
/// [`mounts_of`] does (its device number is an anonymous one); every other
/// filesystem by its mountinfo device number equal to the partition's, and a
/// `fuse*` filesystem is never ours. Same order as [`mounts_of`]: deepest
/// mount point first. [`mounts_of`] itself stays btrfs only, for the RAID-1
/// guard.
pub fn mounts_of_partitions(entries: &[MountEntry], partitions: &[PartitionId]) -> Vec<MountEntry> {
    let paths: Vec<String> = partitions.iter().map(|p| p.path.clone()).collect();
    let devs: BTreeSet<&str> = partitions.iter().map(|p| p.dev.as_str()).collect();
    let mut out: Vec<MountEntry> = entries
        .iter()
        .filter(|e| {
            !e.fstype.starts_with("fuse") && e.fstype != "btrfs" && devs.contains(e.dev.as_str())
        })
        .cloned()
        .collect();
    out.extend(mounts_of(entries, &paths));
    sort_deepest_first(&mut out);
    out
}

fn sort_deepest_first(v: &mut [MountEntry]) {
    v.sort_by(|a, b| {
        b.mount_point
            .matches('/')
            .count()
            .cmp(&a.mount_point.matches('/').count())
            .then_with(|| b.id.cmp(&a.id))
    });
}

fn mounts_matching(
    entries: &[MountEntry],
    paths: &[String],
    kind: impl Fn(&MountEntry) -> bool,
) -> Vec<MountEntry> {
    let mut names: BTreeSet<String> = BTreeSet::new();
    for p in paths {
        names.insert(p.clone());
        if let Ok(c) = fs::canonicalize(p) {
            names.insert(c.to_string_lossy().into_owned());
        }
    }
    let mut out: Vec<MountEntry> = entries
        .iter()
        .filter(|e| {
            kind(e)
                && (names.contains(&e.source)
                    || fs::canonicalize(&e.source)
                        .is_ok_and(|c| names.contains(&*c.to_string_lossy())))
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        b.mount_point
            .matches('/')
            .count()
            .cmp(&a.mount_point.matches('/').count())
            .then_with(|| b.id.cmp(&a.id))
    });
    out
}

/// Mounts of other filesystems stacked under one of `ours`: a refusal, never
/// unmounted by this module (they are not the target).
pub fn stacked_under<'a>(entries: &'a [MountEntry], ours: &[MountEntry]) -> Vec<&'a MountEntry> {
    let ids: BTreeSet<u64> = ours.iter().map(|m| m.id).collect();
    entries
        .iter()
        .filter(|e| !ids.contains(&e.id))
        .filter(|e| {
            ours.iter()
                .any(|m| e.mount_point != m.mount_point && under(&e.mount_point, &m.mount_point))
        })
        .collect()
}

/// Who made a mount, as far as can be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// udisks2, for a desktop session (a mount point under `/run/media`).
    Udisks,
    /// A systemd `.mount` unit that has a unit file or a generator source
    /// (fstab): the unit, and where it comes from.
    Unit { unit: String, from: String },
    /// Neither: a hand mount or another program.
    Other,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Udisks => write!(f, "udisks (a desktop session's automount)"),
            Origin::Unit { unit, from } => write!(f, "systemd unit {unit} ({from})"),
            Origin::Other => write!(f, "a hand mount or another program"),
        }
    }
}

/// Where the mount at `mount_point` came from. `unit_show` is the output of
/// `systemctl show --property=Id --property=LoadState --property=FragmentPath
/// --property=SourcePath <mount_point>`, when that could be run: systemd
/// lists every mount as a unit, so only one with a file or a generator
/// source counts as made by a unit.
pub fn origin(mount_point: &str, unit_show: Option<&str>) -> Origin {
    if under(mount_point, "/run/media") && mount_point != "/run/media" {
        return Origin::Udisks;
    }
    let Some(text) = unit_show else {
        return Origin::Other;
    };
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    if get("LoadState") != Some("loaded") {
        return Origin::Other;
    }
    let Some(unit) = get("Id") else {
        return Origin::Other;
    };
    // A mount nothing declared still has a unit, synthesized from the mount
    // table: `SourcePath=/proc/self/mountinfo`, no fragment (read on a host,
    // 2026-11-25). Only a file or a generator source is a declaration.
    let source = get("SourcePath").filter(|s| *s != "/proc/self/mountinfo");
    match (get("FragmentPath"), source) {
        (Some(f), Some(s)) if f.starts_with("/run/systemd/generator") => Origin::Unit {
            unit: unit.to_string(),
            from: format!("generated from {s}"),
        },
        (Some(f), _) => Origin::Unit {
            unit: unit.to_string(),
            from: f.to_string(),
        },
        (None, Some(s)) => Origin::Unit {
            unit: unit.to_string(),
            from: format!("generated from {s}"),
        },
        (None, None) => Origin::Other,
    }
}

/// Whether `path` is `dir` or inside it, on component boundaries.
fn under(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return path.starts_with('/');
    }
    path == dir || path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

// ---------------------------------------------------------------------------
// Holders
// ---------------------------------------------------------------------------

/// How a process uses a mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    /// Only its working directory, root or executable is there.
    CwdOnly,
    /// It has files there open, or mapped, for reading only.
    Reader,
    /// A file open for writing, a shared writable mapping, or a use that
    /// could not be read: never signalled.
    Writer,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::CwdOnly => "working directory only",
            Mode::Reader => "reading",
            Mode::Writer => "WRITING",
        })
    }
}

/// One process using a mount of the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: i32,
    /// `/proc/<pid>/stat` field 22: what tells a reused pid apart.
    pub start_time: u64,
    pub uid: Option<u32>,
    pub user: Option<String>,
    /// From `/proc/<pid>/cgroup`: the systemd unit it runs in.
    pub unit: Option<String>,
    pub cmdline: String,
    pub mode: Mode,
    pub kernel_thread: bool,
    /// What it holds, one phrase each (`fd 3 /mnt/x/f (read)`, `cwd /mnt/x`).
    pub uses: Vec<String>,
}

impl fmt::Display for Holder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let user = match (&self.user, self.uid) {
            (Some(u), Some(id)) => format!("{u} (uid {id})"),
            (None, Some(id)) => format!("uid {id}"),
            _ => "uid unknown".into(),
        };
        write!(
            f,
            "pid {} [{}] user {user}, unit {}: {} — {}",
            self.pid,
            self.mode,
            self.unit.as_deref().unwrap_or("none"),
            if self.cmdline.is_empty() {
                "(no command line)"
            } else {
                &self.cmdline
            },
            self.uses.join("; ")
        )
    }
}

/// One line of a clearing's account (bd DAS-Backup-Manager-3gzs.14): a kind
/// word (`mount`, `options`, `holder`, `signal`, `action`) in a fixed column,
/// then its facts, so a reader scans down the kinds. The `CLEAR TARGET:` /
/// `CLEAR DRIVE:` status lines are not rows.
pub fn row(kind: &str, body: &str) -> String {
    format!("{kind:<9}{body}")
}

/// The holders of one block as aligned `holder` rows: pid and command padded
/// to the block's widest, then how it uses the mount, who it is, and what
/// the clearing does about it (`disposition`, after `->`). Every fact the
/// holder's `Display` carries is there.
pub fn holder_rows(items: &[(&Holder, String)]) -> Vec<String> {
    let cmd = |h: &Holder| -> String {
        if h.cmdline.is_empty() {
            "(no command line)".into()
        } else {
            h.cmdline.clone()
        }
    };
    let pid_w = items
        .iter()
        .map(|(h, _)| h.pid.to_string().len())
        .max()
        .unwrap_or(0);
    let cmd_w = items
        .iter()
        .map(|(h, _)| cmd(h).chars().count())
        .max()
        .unwrap_or(0)
        .min(40);
    items
        .iter()
        .map(|(h, disposition)| {
            let user = match (&h.user, h.uid) {
                (Some(u), Some(id)) => format!("{u} (uid {id})"),
                (None, Some(id)) => format!("uid {id}"),
                _ => "uid unknown".into(),
            };
            let mode = match h.mode {
                Mode::CwdOnly => "cwd only",
                Mode::Reader => "reader",
                Mode::Writer => "writer",
            };
            let uses = if h.uses.is_empty() {
                String::new()
            } else {
                format!(" ({})", h.uses.join("; "))
            };
            row(
                "holder",
                &format!(
                    "pid {:<pid_w$}  {:<cmd_w$}  {mode}{uses}  user {user}, unit {}  -> {disposition}",
                    h.pid,
                    cmd(h),
                    h.unit.as_deref().unwrap_or("none"),
                ),
            )
        })
        .collect()
}

/// Where the process table is read: `/proc` and `/etc/passwd` in production.
#[derive(Debug, Clone)]
pub struct ProcSite {
    pub proc_root: PathBuf,
    pub passwd: PathBuf,
}

impl ProcSite {
    pub fn host() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            passwd: PathBuf::from("/etc/passwd"),
        }
    }
}

/// `/proc/<pid>/stat`: (state, ppid, flags, start time), parsed after the
/// last `)` (a command name may hold one).
fn read_stat(proc_root: &Path, pid: i32) -> io::Result<(char, i32, u64, u64)> {
    let text = fs::read_to_string(proc_root.join(pid.to_string()).join("stat"))?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed stat");
    let rest = &text[text.rfind(')').ok_or_else(bad)? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    if f.len() < 20 {
        return Err(bad());
    }
    let state = f[0].chars().next().ok_or_else(bad)?;
    let ppid = f[1].parse().map_err(|_| bad())?;
    let flags = f[6].parse().map_err(|_| bad())?;
    let start = f[19].parse().map_err(|_| bad())?;
    Ok((state, ppid, flags, start))
}

/// The start time of `pid` now, `None` when it is gone or a zombie.
pub fn start_time(proc_root: &Path, pid: i32) -> Option<u64> {
    match read_stat(proc_root, pid) {
        Ok((state, _, _, start)) if state != 'Z' && state != 'X' => Some(start),
        _ => None,
    }
}

/// `pid` and every ancestor of it, up to PID 1.
pub fn lineage(proc_root: &Path, pid: i32) -> BTreeSet<i32> {
    let mut out = BTreeSet::new();
    let mut at = pid;
    while at > 0 && out.insert(at) {
        match read_stat(proc_root, at) {
            Ok((_, ppid, _, _)) => at = ppid,
            Err(_) => break,
        }
    }
    out
}

/// The id of the mount `path` is on, following links (statx `STATX_MNT_ID`,
/// Linux 5.8): the id mountinfo's first field and fdinfo's `mnt_id` carry.
/// `Ok(None)` when the kernel does not report it.
pub(crate) fn mount_id_of(path: &Path) -> io::Result<Option<u64>> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    // SAFETY: a zeroed statx is a valid out-buffer; the path is NUL-terminated.
    let mut buf: libc::statx = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statx(libc::AT_FDCWD, c.as_ptr(), 0, libc::STATX_MNT_ID, &mut buf) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(reported_mnt_id(buf.stx_mask, buf.stx_mnt_id))
}

/// The mount id statx returned, when its mask says the kernel filled it in.
fn reported_mnt_id(mask: u32, id: u64) -> Option<u64> {
    (mask & libc::STATX_MNT_ID != 0).then_some(id)
}

/// What a link in `/proc/<pid>` names, without a " (deleted)" suffix.
fn link_path(p: &Path) -> io::Result<String> {
    let t = fs::read_link(p)?.to_string_lossy().into_owned();
    Ok(t.strip_suffix(" (deleted)")
        .map(str::to_string)
        .unwrap_or(t))
}

/// fdinfo's `flags:` (octal) and `mnt_id:`.
fn read_fdinfo(path: &Path) -> io::Result<(u32, Option<u64>)> {
    let text = fs::read_to_string(path)?;
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "no flags line");
    let flags = text
        .lines()
        .find_map(|l| l.strip_prefix("flags:"))
        .and_then(|v| u32::from_str_radix(v.trim(), 8).ok())
        .ok_or_else(bad)?;
    let mnt = text
        .lines()
        .find_map(|l| l.strip_prefix("mnt_id:"))
        .and_then(|v| v.trim().parse().ok());
    Ok((flags, mnt))
}

fn is_gone(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH)
}

fn user_name(passwd: &Path, uid: u32) -> Option<String> {
    let text = fs::read_to_string(passwd).ok()?;
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 2 && f[2].parse::<u32>().ok() == Some(uid)).then(|| f[0].to_string())
    })
}

fn unit_of(cgroup: &str) -> Option<String> {
    const SUFFIXES: [&str; 6] = [".service", ".scope", ".mount", ".socket", ".swap", ".slice"];
    let path = cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .or_else(|| cgroup.lines().next().and_then(|l| l.rsplit(':').next()))?;
    path.rsplit('/')
        .find(|c| SUFFIXES.iter().any(|s| c.ends_with(s)) && !c.ends_with(".slice"))
        .map(str::to_string)
        .or_else(|| Some(path.to_string()).filter(|p| !p.is_empty()))
}

/// Every process using one of `mounts`. A process that vanishes while it is
/// read is left out; one whose use cannot be read is a [`Mode::Writer`],
/// never assumed harmless. `Err` only when the process table itself cannot
/// be listed.
pub fn scan_holders(site: &ProcSite, mounts: &[MountEntry]) -> Result<Vec<Holder>, String> {
    let ids: BTreeSet<u64> = mounts.iter().map(|m| m.id).collect();
    let points: Vec<&str> = mounts.iter().map(|m| m.mount_point.as_str()).collect();
    let on = |p: &str| points.iter().any(|m| under(p, m));
    let entries =
        fs::read_dir(&site.proc_root).map_err(|e| format!("{}: {e}", site.proc_root.display()))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", site.proc_root.display()))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        let Ok((state, _, flags, start)) = read_stat(&site.proc_root, pid) else {
            continue;
        };
        if state == 'Z' || state == 'X' {
            continue;
        }
        let mut mode: Option<Mode> = None;
        let mut uses = Vec::new();
        let mut note = |m: Mode, what: String, mode: &mut Option<Mode>| {
            *mode = Some(mode.map_or(m, |o| o.max(m)));
            uses.push(what);
        };
        // A working directory, root or executable counts by the mount it is
        // on (statx STATX_MNT_ID through the /proc link), never by its path:
        // a shell in the bare directory under a mount, or a path that only
        // reads alike in another namespace, is not a holder (bd su1b.12
        // review). One whose mount cannot be told but whose path is there is
        // unknown: a writer, never signalled.
        for (name, what) in [("cwd", "cwd"), ("root", "root"), ("exe", "executable")] {
            let link = dir.join(name);
            let shown = link_path(&link).unwrap_or_else(|_| "?".into());
            match mount_id_of(&link) {
                Ok(Some(id)) => {
                    if ids.contains(&id) {
                        note(Mode::CwdOnly, format!("{what} {shown}"), &mut mode);
                    }
                }
                Err(e) if is_gone(&e) => {}
                other => {
                    if on(&shown) {
                        let why = match other {
                            Err(e) => e.to_string(),
                            _ => "the kernel reports no mount id".into(),
                        };
                        note(
                            Mode::Writer,
                            format!("{what} {shown} (its mount cannot be told: {why})"),
                            &mut mode,
                        );
                    }
                }
            }
        }
        match fs::read_dir(dir.join("fd")) {
            Ok(fds) => {
                for fd in fds.flatten() {
                    let n = fd.file_name().to_string_lossy().into_owned();
                    let target = link_path(&fd.path());
                    let info = read_fdinfo(&dir.join("fdinfo").join(&n));
                    let path = target.as_deref().unwrap_or("?");
                    let ours = match &info {
                        Ok((_, Some(mnt))) => ids.contains(mnt),
                        _ => on(path),
                    };
                    if !ours {
                        continue;
                    }
                    match info {
                        Ok((fl, _)) if fl & 0o3 == 0 => {
                            note(Mode::Reader, format!("fd {n} {path} (read)"), &mut mode)
                        }
                        Ok(_) => note(Mode::Writer, format!("fd {n} {path} (WRITE)"), &mut mode),
                        // Closed meanwhile: the fd is gone too. An fd still
                        // there whose fdinfo is not is unknown: a writer.
                        Err(e) if is_gone(&e) && fs::symlink_metadata(fd.path()).is_err() => {}
                        Err(e) => note(
                            Mode::Writer,
                            format!("fd {n} {path} (open mode unreadable: {e})"),
                            &mut mode,
                        ),
                    }
                }
            }
            Err(e) if is_gone(&e) => continue,
            Err(e) => note(
                Mode::Writer,
                format!("its open files cannot be read ({e})"),
                &mut mode,
            ),
        }
        let maps = match fs::read_to_string(dir.join("maps")) {
            Ok(m) => m,
            Err(e) if is_gone(&e) => continue,
            Err(e) => {
                note(
                    Mode::Writer,
                    format!("its mappings cannot be read ({e})"),
                    &mut mode,
                );
                String::new()
            }
        };
        {
            for line in maps.lines() {
                let f: Vec<&str> = line.splitn(6, char::is_whitespace).collect();
                if f.len() < 6 {
                    continue;
                }
                let path = f[5].trim();
                let path = path.strip_suffix(" (deleted)").unwrap_or(path);
                if !path.starts_with('/') || !on(path) {
                    continue;
                }
                let perms = f[1].as_bytes();
                if perms.len() >= 4 && perms[1] == b'w' && perms[3] == b's' {
                    note(
                        Mode::Writer,
                        format!("shared writable map {path}"),
                        &mut mode,
                    );
                } else {
                    note(Mode::Reader, format!("map {path}"), &mut mode);
                }
            }
        }
        let Some(mut mode) = mode else {
            continue;
        };
        // btrfs maintenance (balance, replace, device add/remove, resize,
        // defrag, scrub) and btrbk work through ioctls on a read-only fd:
        // never a reader to be stopped (bd su1b.12 review).
        let comm = fs::read_to_string(dir.join("comm")).unwrap_or_default();
        let exe = link_path(&dir.join("exe")).unwrap_or_default();
        let argv0 = fs::read(dir.join("cmdline"))
            .ok()
            .and_then(|b| {
                b.split(|c| *c == 0)
                    .next()
                    .map(|a| String::from_utf8_lossy(a).into_owned())
            })
            .unwrap_or_default();
        let base = |s: &str| s.trim().rsplit('/').next().unwrap_or("").to_string();
        if [comm.trim().to_string(), base(&exe), base(&argv0)]
            .iter()
            .any(|n| n == "btrfs" || n == "btrbk")
        {
            mode = Mode::Writer;
            uses.push(format!(
                "a btrfs maintenance tool ({}): its work goes through ioctls",
                comm.trim()
            ));
        }
        uses.sort();
        uses.dedup();
        let status = fs::read_to_string(dir.join("status")).unwrap_or_default();
        let uid = status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok());
        let cmdline = fs::read(dir.join("cmdline"))
            .map(|b| display_cmdline(&b))
            .unwrap_or_default();
        out.push(Holder {
            pid,
            start_time: start,
            uid,
            user: uid.and_then(|u| user_name(&site.passwd, u)),
            unit: fs::read_to_string(dir.join("cgroup"))
                .ok()
                .and_then(|c| unit_of(&c)),
            cmdline,
            mode,
            kernel_thread: flags & PF_KTHREAD != 0,
            uses,
        });
    }
    out.sort_by_key(|h| h.pid);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Clearing
// ---------------------------------------------------------------------------

/// The two signals the policy sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sig {
    Term,
    Kill,
}

impl Sig {
    pub fn name(self) -> &'static str {
        match self {
            Sig::Term => "SIGTERM",
            Sig::Kill => "SIGKILL",
        }
    }
}

/// Sends a signal to one process whose start time was just read as
/// `start_time`. Never to a pid of 1 or less.
pub trait Signaller {
    fn signal(&self, pid: i32, start_time: u64, sig: Sig) -> Result<(), String>;
}

/// The host's signaller: a pidfd opened first, the start time read again
/// through `/proc`, then the signal sent through that pidfd, so a pid reused
/// between the check and the signal cannot be hit. Refuses in every test
/// build (`test-overrides`, as `report::MAILX` does).
pub struct SystemSignaller;

impl Signaller for SystemSignaller {
    fn signal(&self, pid: i32, expected: u64, sig: Sig) -> Result<(), String> {
        if cfg!(feature = "test-overrides") {
            return Err(format!(
                "refused: a test build never signals a real process (pid {pid})"
            ));
        }
        signal_process(Path::new("/proc"), pid, expected, sig)
    }
}

/// The signalling itself, with the `/proc` its start-time check reads given:
/// [`SystemSignaller`] minus the test-build refusal, so the tests can drive
/// it against processes they spawned themselves.
fn signal_process(proc_root: &Path, pid: i32, expected: u64, sig: Sig) -> Result<(), String> {
    if pid <= 1 {
        return Err(format!("refused: never a signal to pid {pid}"));
    }
    // SAFETY: plain syscalls on integer arguments; the fd is closed below.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(format!("pidfd_open({pid}): {}", io::Error::last_os_error()));
    }
    let fd = fd as libc::c_int;
    let result = if start_time(proc_root, pid) != Some(expected) {
        Err(format!(
            "pid {pid} is no longer the process that was checked"
        ))
    } else {
        let signo = match sig {
            Sig::Term => libc::SIGTERM,
            Sig::Kill => libc::SIGKILL,
        };
        // SAFETY: as above; a null siginfo is allowed.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd,
                signo,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!(
                "pidfd_send_signal({pid}, {}): {}",
                sig.name(),
                io::Error::last_os_error()
            ))
        }
    };
    // SAFETY: fd is ours.
    unsafe { libc::close(fd) };
    result
}

/// What [`clear_holders`] did: one line per holder seen and per signal sent,
/// for the job's log and report.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cleared {
    pub lines: Vec<String>,
    /// The signals sent: (pid, signal, command line).
    pub signalled: Vec<(i32, Sig, String)>,
    /// Who each signalled pid is, for the report (`user bosco, unit
    /// app-dolphin.scope`; bd su1b.31).
    pub who: std::collections::BTreeMap<i32, String>,
    /// Report-only: the holders it would stop, one line each.
    pub would_stop: Vec<String>,
}

/// Why [`clear_holders`] refused: `lines` as far as it got, `why` naming the
/// holders that decided it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub lines: Vec<String>,
    pub why: String,
}

/// The host-independent inputs of [`clear_holders`].
pub struct ClearEnv<'a> {
    pub site: &'a ProcSite,
    /// This process: it and its ancestors are never signalled.
    pub self_pid: i32,
    pub signaller: &'a dyn Signaller,
    pub sleep: &'a dyn Fn(Duration),
    /// A dry run (bd su1b.12 review): every holder classified and said
    /// ("would stop: …"), the same refusals, and no signal at all.
    pub report_only: bool,
    /// Whether this job may stop readers at all (operator 2026-11-25): only
    /// a job that writes under the target does. `false` → any holder
    /// refuses, named, and nothing is signalled.
    pub stop_readers: bool,
}

/// A holder no signal may reach: why, or `None`.
fn protected(h: &Holder, mine: &BTreeSet<i32>) -> Option<&'static str> {
    if h.pid <= 1 {
        Some("PID 1")
    } else if h.kernel_thread {
        Some("a kernel thread")
    } else if mine.contains(&h.pid) {
        Some("this job or one of its ancestors")
    } else {
        None
    }
}

/// Apply the stop policy to every process using one of `mounts` (see the
/// module text). `Ok` when nothing uses them any more; `Err` names what
/// refused it.
pub fn clear_holders(env: &ClearEnv<'_>, mounts: &[MountEntry]) -> Result<Cleared, Refused> {
    let mut done = Cleared::default();
    let refuse = |lines: Vec<String>, why: String| Refused { lines, why };
    let scan = |lines: &[String]| {
        scan_holders(env.site, mounts).map_err(|e| Refused {
            lines: lines.to_vec(),
            why: format!("who uses it cannot be read ({e})"),
        })
    };
    let holders = scan(&done.lines)?;
    if holders.is_empty() {
        return Ok(done);
    }
    let mine = lineage(&env.site.proc_root, env.self_pid);
    let first: Vec<(&Holder, String)> = holders
        .iter()
        .map(|h| {
            let disposition = if let Some(w) = protected(h, &mine) {
                format!("not signalled ({w})")
            } else if h.mode == Mode::Writer {
                "not signalled".to_string()
            } else if !env.stop_readers {
                "not signalled (this job stops no process)".to_string()
            } else if env.report_only {
                "would stop".to_string()
            } else {
                "to be stopped".to_string()
            };
            (h, disposition)
        })
        .collect();
    done.lines.extend(holder_rows(&first));
    // Decide first: anything that refuses refuses before any signal.
    let check = |hs: &[Holder]| -> Option<String> {
        let blocked: Vec<String> = hs
            .iter()
            .filter_map(|h| protected(h, &mine).map(|w| format!("pid {} ({w})", h.pid)))
            .collect();
        if !blocked.is_empty() {
            return Some(format!(
                "it is used by {}, which is never signalled",
                blocked.join(", ")
            ));
        }
        let writers: Vec<String> = hs
            .iter()
            .filter(|h| h.mode == Mode::Writer)
            .map(|h| {
                format!(
                    "pid {} ({}: {})",
                    h.pid,
                    if h.cmdline.is_empty() {
                        "?"
                    } else {
                        &h.cmdline
                    },
                    h.uses.join("; ")
                )
            })
            .collect();
        (!writers.is_empty()).then(|| {
            format!(
                "it is held open for writing by {} — never killed; close it, then run again",
                writers.join(", ")
            )
        })
    };
    if let Some(why) = check(&holders) {
        return Err(refuse(done.lines, why));
    }
    if !env.stop_readers {
        let named: Vec<String> = holders
            .iter()
            .map(|h| {
                format!(
                    "pid {} ({})",
                    h.pid,
                    if h.cmdline.is_empty() {
                        "?"
                    } else {
                        &h.cmdline
                    }
                )
            })
            .collect();
        return Err(refuse(
            done.lines,
            format!(
                "it is in use by {} — this job writes nothing to the target and never stops \
                 a process; close it, then run again",
                named.join(", ")
            ),
        ));
    }
    if env.report_only {
        done.would_stop = holders.iter().map(|h| format!("would stop: {h}")).collect();
        return Ok(done);
    }
    fn still(hs: &[Holder]) -> Vec<(&Holder, String)> {
        hs.iter()
            .map(|h| (h, "still holding".to_string()))
            .collect()
    }
    let signal_all = |hs: &[Holder], sig: Sig, done: &mut Cleared| {
        for h in hs {
            if start_time(&env.site.proc_root, h.pid) != Some(h.start_time) {
                done.lines.push(row(
                    "signal",
                    &format!(
                        "{} pid {} not sent: gone or reused before it was sent",
                        sig.name(),
                        h.pid
                    ),
                ));
                continue;
            }
            match env.signaller.signal(h.pid, h.start_time, sig) {
                Ok(()) => {
                    let who = format!(
                        "user {}, unit {}",
                        match (&h.user, h.uid) {
                            (Some(u), _) => u.clone(),
                            (None, Some(id)) => format!("uid {id}"),
                            _ => "unknown".into(),
                        },
                        h.unit.as_deref().unwrap_or("none")
                    );
                    done.lines.push(row(
                        "signal",
                        &format!("{} pid {} ({}, {who})", sig.name(), h.pid, h.cmdline),
                    ));
                    done.signalled.push((h.pid, sig, h.cmdline.clone()));
                    done.who.insert(h.pid, who);
                }
                Err(e) => done.lines.push(row(
                    "signal",
                    &format!("{} pid {} not sent: {e}", sig.name(), h.pid),
                )),
            }
        }
    };
    signal_all(&holders, Sig::Term, &mut done);
    let wait = |limit: Duration, done: &Cleared| -> Result<Vec<Holder>, Refused> {
        let mut waited = Duration::ZERO;
        loop {
            let now = scan(&done.lines)?;
            if now.is_empty() || waited >= limit {
                return Ok(now);
            }
            (env.sleep)(POLL);
            waited += POLL;
        }
    };
    let left = wait(TERM_GRACE, &done)?;
    if left.is_empty() {
        return Ok(done);
    }
    // Classified again: one that began writing, or a new holder of a kind
    // that refuses, stops here.
    if let Some(why) = check(&left) {
        let rows = holder_rows(&still(&left));
        done.lines.extend(rows);
        return Err(refuse(done.lines, why));
    }
    // A holder that appeared during the grace gets SIGTERM and one grace of
    // its own before anything is killed.
    let newcomers: Vec<Holder> = left
        .iter()
        .filter(|h| {
            !done.signalled.iter().any(|(pid, _, _)| *pid == h.pid)
                || !holders
                    .iter()
                    .any(|o| o.pid == h.pid && o.start_time == h.start_time)
        })
        .cloned()
        .collect();
    let left = if newcomers.is_empty() {
        left
    } else {
        let rows = holder_rows(
            &newcomers
                .iter()
                .map(|h| (h, "new, appeared while clearing".to_string()))
                .collect::<Vec<_>>(),
        );
        done.lines.extend(rows);
        signal_all(&newcomers, Sig::Term, &mut done);
        let again = wait(TERM_GRACE, &done)?;
        if let Some(why) = check(&again) {
            let rows = holder_rows(&still(&again));
            done.lines.extend(rows);
            return Err(refuse(done.lines, why));
        }
        again
    };
    if left.is_empty() {
        return Ok(done);
    }
    signal_all(&left, Sig::Kill, &mut done);
    let left = wait(KILL_WAIT, &done)?;
    if left.is_empty() {
        return Ok(done);
    }
    let names: Vec<String> = left.iter().map(|h| format!("pid {}", h.pid)).collect();
    let rows = holder_rows(&still(&left));
    done.lines.extend(rows);
    Err(refuse(
        done.lines,
        format!(
            "it is still used by {} after SIGTERM and SIGKILL",
            names.join(", ")
        ),
    ))
}

#[cfg(test)]
pub(crate) mod testproc {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A fake `/proc`, built in a temporary directory.
    pub(crate) struct FakeProc {
        dir: tempfile::TempDir,
    }

    pub(crate) struct Proc<'a> {
        pub(crate) pid: i32,
        pub(crate) ppid: i32,
        pub(crate) start: u64,
        pub(crate) flags: u64,
        pub(crate) state: char,
        pub(crate) cmd: &'a str,
        /// `/proc/<pid>/comm`.
        pub(crate) comm: &'a str,
        pub(crate) uid: u32,
        pub(crate) cgroup: &'a str,
        pub(crate) cwd: &'a str,
        /// fd number, target, flags (octal text), mnt_id
        pub(crate) fds: Vec<(u32, &'a str, &'a str, Option<u64>)>,
        pub(crate) maps: &'a str,
    }

    impl Default for Proc<'_> {
        fn default() -> Self {
            Self {
                pid: 0,
                ppid: 1,
                start: 100,
                flags: 0x0040_0000,
                state: 'S',
                cmd: "",
                comm: "x",
                uid: 1000,
                cgroup: "0::/user.slice/user-1000.slice/session-2.scope\n",
                cwd: "/home/user",
                fds: Vec::new(),
                maps: "",
            }
        }
    }

    impl FakeProc {
        pub(crate) fn new() -> Self {
            let f = Self {
                dir: tempfile::TempDir::new().unwrap(),
            };
            fs::create_dir_all(f.dir.path().join("proc")).unwrap();
            fs::write(
                f.dir.path().join("passwd"),
                "root:x:0:0::/root:/bin/bash\nuser:x:1000:1000::/home/user:/bin/bash\n",
            )
            .unwrap();
            f
        }
        pub(crate) fn site(&self) -> ProcSite {
            ProcSite {
                proc_root: self.dir.path().join("proc"),
                passwd: self.dir.path().join("passwd"),
            }
        }
        pub(crate) fn add(&self, p: Proc<'_>) {
            let d = self.dir.path().join("proc").join(p.pid.to_string());
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(d.join("fd")).unwrap();
            fs::create_dir_all(d.join("fdinfo")).unwrap();
            // Fields 3..: state ppid pgrp session tty tpgid flags minflt
            // cminflt majflt cmajflt utime stime cutime cstime priority nice
            // threads itrealvalue starttime.
            fs::write(
                d.join("stat"),
                format!(
                    "{} (a (b) c) {} {} 0 0 0 0 {} 0 0 0 0 0 0 0 0 20 0 1 0 {} 0 0\n",
                    p.pid, p.state, p.ppid, p.flags, p.start
                ),
            )
            .unwrap();
            fs::write(
                d.join("status"),
                format!("Name:\tx\nUid:\t{0}\t{0}\t{0}\t{0}\n", p.uid),
            )
            .unwrap();
            fs::write(d.join("cmdline"), p.cmd.replace(' ', "\0")).unwrap();
            fs::write(d.join("comm"), format!("{}\n", p.comm)).unwrap();
            fs::write(d.join("cgroup"), p.cgroup).unwrap();
            fs::write(d.join("maps"), p.maps).unwrap();
            symlink(p.cwd, d.join("cwd")).unwrap();
            symlink("/", d.join("root")).unwrap();
            symlink("/usr/bin/x", d.join("exe")).unwrap();
            for (n, target, flags, mnt) in p.fds {
                symlink(target, d.join("fd").join(n.to_string())).unwrap();
                let mut info = format!("pos:\t0\nflags:\t{flags}\n");
                if let Some(m) = mnt {
                    info.push_str(&format!("mnt_id:\t{m}\n"));
                }
                fs::write(d.join("fdinfo").join(n.to_string()), info).unwrap();
            }
        }
        /// The mount id of the real mount holding `path` (statx
        /// `STATX_MNT_ID`): the id a test's mount entry must carry for a
        /// working directory there to count.
        pub(crate) fn real_mount_id(path: &Path) -> u64 {
            super::mount_id_of(path).unwrap().expect("STATX_MNT_ID")
        }

        pub(crate) fn remove(&self, pid: i32) {
            fs::remove_dir_all(self.dir.path().join("proc").join(pid.to_string())).unwrap();
        }
    }
}

/// The longest command line shown for a holder, in characters.
pub const DISPLAY_CMDLINE_MAX: usize = 160;

/// A process's `/proc/<pid>/cmdline` as it is shown in the log, the
/// `clear-target` lines and the emailed report: arguments joined by spaces,
/// every control character (a newline that would forge a report line, an
/// escape sequence) replaced by `?`, cut to [`DISPLAY_CMDLINE_MAX`]
/// characters ending in `…`. Any local user writes their own argv, so it is
/// text from outside, never trusted (security scan of su1b.31).
pub fn display_cmdline(raw: &[u8]) -> String {
    let joined = raw
        .split(|c| *c == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let clean: Vec<char> = joined
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if clean.len() <= DISPLAY_CMDLINE_MAX {
        clean.into_iter().collect()
    } else {
        let mut cut: String = clean[..DISPLAY_CMDLINE_MAX - 1].iter().collect();
        cut.push('\u{2026}');
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::testproc::{FakeProc, Proc};
    use super::*;

    #[test]
    fn a_command_line_cannot_forge_a_report_line_and_is_bounded() {
        // A process's argv is any local user's text, and it reaches the
        // emailed report: control characters become '?', the whole is cut
        // to DISPLAY_CMDLINE_MAX characters.
        let shown = display_cmdline(b"evil\nTarget primary  OK\0--flag\0\x1b[31mred\0\0");
        assert_eq!(shown, "evil?Target primary  OK --flag ?[31mred");
        let long = display_cmdline("x".repeat(500).as_bytes());
        assert_eq!(long.chars().count(), DISPLAY_CMDLINE_MAX);
        assert!(long.ends_with('\u{2026}'));
        assert_eq!(display_cmdline(b""), "");
        // Not UTF-8: replaced, never a panic.
        assert_eq!(display_cmdline(b"a\xffb"), "a\u{fffd}b");
    }
    use std::cell::RefCell;
    use std::os::unix::fs::symlink;

    fn das_mount() -> Vec<MountEntry> {
        parse_mountinfo(
            "77 30 0:52 / /run/media/user/das rw,nosuid,nodev - btrfs /dev/sdx1 rw,space_cache=v2,subvolid=5,subvol=/\n",
        )
    }

    #[test]
    fn mountinfo_lists_every_mount_of_a_member_deepest_first() {
        let text = "\
22 1 0:21 / / rw - btrfs /dev/nvme0n1p2 rw\n\
77 30 0:52 / /run/media/user/das rw - btrfs /dev/sdx1 rw,subvolid=5\n\
78 30 0:52 /@snap /mnt/a\\040b rw - btrfs /dev/sdy1 rw,subvolid=256\n\
79 77 0:53 / /run/media/user/das/inner rw - ext4 /dev/sdz1 rw\n";
        let all = parse_mountinfo(text);
        assert_eq!(all.len(), 4);
        let ours = mounts_of(&all, &["/dev/sdx1".into(), "/dev/sdy1".into()]);
        let points: Vec<&str> = ours.iter().map(|m| m.mount_point.as_str()).collect();
        assert_eq!(points, vec!["/run/media/user/das", "/mnt/a b"]);
        let stacked = stacked_under(&all, &ours);
        assert_eq!(stacked.len(), 1);
        assert_eq!(stacked[0].mount_point, "/run/media/user/das/inner");
    }

    fn part(path: &str, dev: &str) -> PartitionId {
        PartitionId {
            path: path.into(),
            dev: dev.into(),
        }
    }

    #[test]
    fn mounts_of_partitions_matches_vfat_and_btrfs_by_partition() {
        let text = "\
22 1 0:21 / / rw - btrfs /dev/nvme0n1p2 rw\n\
50 30 8:17 / /run/media/user/ESP rw - vfat /dev/sdx1 rw,fmask=0022\n\
51 30 0:61 / /mnt/b rw - btrfs /dev/sdx2 rw,subvolid=5\n\
52 50 8:17 / /run/media/user/ESP/inner rw - vfat /dev/sdx1 rw\n";
        let all = parse_mountinfo(text);
        let got = mounts_of_partitions(
            &all,
            &[part("/dev/sdx1", "8:17"), part("/dev/sdx2", "8:18")],
        );
        let points: Vec<&str> = got.iter().map(|m| m.mount_point.as_str()).collect();
        // Any filesystem type; deepest mount point first.
        assert_eq!(
            points,
            vec!["/run/media/user/ESP/inner", "/run/media/user/ESP", "/mnt/b"]
        );
        // `mounts_of` stays btrfs only, for the RAID-1 guard.
        let btrfs = mounts_of(&all, &["/dev/sdx1".into(), "/dev/sdx2".into()]);
        assert_eq!(btrfs.len(), 1);
        assert_eq!(btrfs[0].mount_point, "/mnt/b");
    }

    #[test]
    fn mounts_of_partitions_ignores_other_partitions() {
        let all = parse_mountinfo(
            "50 30 8:33 / /run/media/user/ESP rw - vfat /dev/sdy1 rw\n\
51 30 8:26 / /mnt/b rw - vfat /dev/sdx10 rw\n",
        );
        assert!(mounts_of_partitions(&all, &[part("/dev/sdx1", "8:17")]).is_empty());
        assert!(mounts_of_partitions(&all, &[]).is_empty());
    }

    /// A user can FUSE-mount anything under any `fsname`, `/dev/sdx1`
    /// included: the source text proves nothing, and root must never unmount
    /// that user's mount or signal their processes for it.
    #[test]
    fn mounts_of_partitions_ignores_fuse_mount_with_forged_source() {
        let all = parse_mountinfo(
            "70 30 0:80 / /home/user/forged rw,nosuid,nodev - fuse.sshfs /dev/sdx1 rw,user_id=1000\n\
71 30 8:17 / /home/user/forged2 rw - fuse /dev/sdx1 rw\n\
72 30 0:81 / /home/user/forged3 rw - vfat /dev/sdx1 rw\n",
        );
        // 70: device number differs and it is FUSE; 71: even a matching
        // number is FUSE, never ours; 72: right source text, wrong device.
        assert!(mounts_of_partitions(&all, &[part("/dev/sdx1", "8:17")]).is_empty());
    }

    #[test]
    fn mounts_of_partitions_matches_by_major_minor_not_source() {
        // udisks reports `/dev/mapper/…` or a by-uuid name for the same
        // device; the number is what the kernel means.
        let all = parse_mountinfo(
            "50 30 8:17 / /run/media/user/ESP rw - vfat /dev/disk/by-uuid/ABCD-0001 rw\n",
        );
        let got = mounts_of_partitions(&all, &[part("/dev/sdx1", "8:17")]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].dev, "8:17");
    }

    #[test]
    fn origin_tells_udisks_a_unit_and_a_hand_mount_apart() {
        assert_eq!(origin("/run/media/user/das", None), Origin::Udisks);
        let unit = "Id=mnt-x.mount\nLoadState=loaded\nFragmentPath=\nSourcePath=/etc/fstab\n";
        assert_eq!(
            origin("/mnt/x", Some(unit)),
            Origin::Unit {
                unit: "mnt-x.mount".into(),
                from: "generated from /etc/fstab".into()
            }
        );
        // What systemd shows for a mount nothing declared (read on a host,
        // 2026-11-25): a unit synthesized from the mount table.
        let synthesized =
            "Id=mnt-x.mount\nLoadState=loaded\nFragmentPath=\nSourcePath=/proc/self/mountinfo\n";
        assert_eq!(origin("/mnt/x", Some(synthesized)), Origin::Other);
        let fstab = "Id=mnt-x.mount\nLoadState=loaded\nFragmentPath=/run/systemd/generator/mnt-x.mount\nSourcePath=/etc/fstab\n";
        assert_eq!(
            origin("/mnt/x", Some(fstab)),
            Origin::Unit {
                unit: "mnt-x.mount".into(),
                from: "generated from /etc/fstab".into()
            }
        );
        assert_eq!(origin("/mnt/x", None), Origin::Other);
        assert_eq!(origin("/run/mediax/y", None), Origin::Other);
    }

    #[test]
    fn holders_are_found_by_mount_id_and_path_and_classified_by_open_mode() {
        let p = FakeProc::new();
        // A reader by mnt_id (octal 0100000 = O_RDONLY|O_LARGEFILE).
        p.add(Proc {
            pid: 200,
            cmd: "dolphin --x",
            fds: vec![(3, "/run/media/user/das/a", "0100000", Some(77))],
            ..Default::default()
        });
        // A writer: 0100002 = O_RDWR.
        p.add(Proc {
            pid: 201,
            cmd: "vim f",
            fds: vec![(4, "/run/media/user/das/f", "0100002", Some(77))],
            ..Default::default()
        });
        // Same path, another mount id: not ours.
        p.add(Proc {
            pid: 202,
            fds: vec![(3, "/run/media/user/das/a", "0100002", Some(99))],
            ..Default::default()
        });
        // Only its working directory there: a real directory, on a mount
        // the scan is given by its real id (a cwd counts by its mount id).
        let real = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(real.path().join("sub")).unwrap();
        let real_dir = fs::canonicalize(real.path()).unwrap();
        let real_sub = Box::leak(
            real_dir
                .join("sub")
                .to_string_lossy()
                .into_owned()
                .into_boxed_str(),
        );
        p.add(Proc {
            pid: 203,
            cmd: "bash",
            cwd: real_sub,
            ..Default::default()
        });
        // A shared writable mapping, no fd.
        p.add(Proc {
            pid: 204,
            maps: "7f00-7f10 rw-s 00000000 00:34 12 /run/media/user/das/db (deleted)\n",
            ..Default::default()
        });
        // A fd whose open mode cannot be read: a writer.
        p.add(Proc {
            pid: 205,
            ..Default::default()
        });
        let d = p.site().proc_root.join("205");
        symlink("/run/media/user/das/x", d.join("fd/5")).unwrap();
        // A path that only shares a prefix.
        p.add(Proc {
            pid: 206,
            cwd: "/run/media/user/dasx",
            ..Default::default()
        });
        // A zombie holds nothing.
        p.add(Proc {
            pid: 207,
            state: 'Z',
            fds: vec![(3, "/run/media/user/das/a", "0100000", Some(77))],
            ..Default::default()
        });
        let mut mounts = das_mount();
        mounts.push(MountEntry {
            id: FakeProc::real_mount_id(&real_dir),
            root: "/".into(),
            mount_point: real_dir.to_string_lossy().into_owned(),
            options: "rw".into(),
            fstype: "btrfs".into(),
            source: "/dev/sdx1".into(),
            dev: "0:52".into(),
            super_options: String::new(),
        });
        let hs = scan_holders(&p.site(), &mounts).unwrap();
        let got: Vec<(i32, Mode)> = hs.iter().map(|h| (h.pid, h.mode)).collect();
        assert_eq!(
            got,
            vec![
                (200, Mode::Reader),
                (201, Mode::Writer),
                (203, Mode::CwdOnly),
                (204, Mode::Writer),
                (205, Mode::Writer),
            ]
        );
        let h = &hs[0];
        assert_eq!(h.user.as_deref(), Some("user"));
        assert_eq!(h.unit.as_deref(), Some("session-2.scope"));
        assert_eq!(h.cmdline, "dolphin --x");
        assert_eq!(h.start_time, 100);
    }

    /// Records each signal, and makes the process exit on the signal its
    /// `dies_on` names.
    struct Fake<'a> {
        proc: &'a FakeProc,
        dies_on: Vec<(i32, Sig)>,
        sent: RefCell<Vec<(i32, Sig)>>,
    }

    impl Signaller for Fake<'_> {
        fn signal(&self, pid: i32, _start: u64, sig: Sig) -> Result<(), String> {
            assert!(pid > 1, "a signal to pid {pid}");
            self.sent.borrow_mut().push((pid, sig));
            if self.dies_on.contains(&(pid, sig)) {
                self.proc.remove(pid);
            }
            Ok(())
        }
    }

    fn reader(p: &FakeProc, pid: i32) {
        p.add(Proc {
            pid,
            cmd: "reader",
            fds: vec![(3, "/run/media/user/das/a", "0100000", Some(77))],
            ..Default::default()
        });
    }

    fn env<'a>(
        p: &'a ProcSite,
        s: &'a dyn Signaller,
        sleeps: &'a dyn Fn(Duration),
    ) -> ClearEnv<'a> {
        ClearEnv {
            site: p,
            self_pid: 900,
            signaller: s,
            sleep: sleeps,
            report_only: false,
            stop_readers: true,
        }
    }

    /// A job that writes nothing under the target (verify, walk, restore,
    /// browse, doctor) never stops a reader: the reader refuses it, named
    /// (operator 2026-11-25), in a dry run too.
    #[test]
    fn a_job_that_may_not_stop_readers_refuses_naming_them_and_signals_nothing() {
        let p = FakeProc::new();
        reader(&p, 300);
        let f = Fake {
            proc: &p,
            dies_on: vec![(300, Sig::Term)],
            sent: RefCell::new(Vec::new()),
        };
        let site = p.site();
        for report_only in [false, true] {
            let mut e = env(&site, &f, &|_| {});
            e.stop_readers = false;
            e.report_only = report_only;
            let r = clear_holders(&e, &das_mount()).unwrap_err();
            assert!(
                f.sent.borrow().is_empty(),
                "nothing signalled: {:?}",
                f.sent
            );
            assert!(
                r.why.contains("pid 300") && r.why.contains("never stops"),
                "{}",
                r.why
            );
        }
    }

    #[test]
    fn readers_get_sigterm_then_sigkill_and_every_signal_is_named() {
        let p = FakeProc::new();
        p.add(Proc {
            pid: 900,
            ppid: 899,
            ..Default::default()
        });
        reader(&p, 300);
        reader(&p, 301);
        let f = Fake {
            proc: &p,
            dies_on: vec![(300, Sig::Term), (301, Sig::Kill)],
            sent: RefCell::new(Vec::new()),
        };
        let slept = RefCell::new(Duration::ZERO);
        let sleep = |d: Duration| *slept.borrow_mut() += d;
        let site = p.site();
        let done = clear_holders(&env(&site, &f, &sleep), &das_mount()).unwrap();
        assert_eq!(
            *f.sent.borrow(),
            vec![(300, Sig::Term), (301, Sig::Term), (301, Sig::Kill)]
        );
        assert_eq!(
            *slept.borrow(),
            TERM_GRACE,
            "the whole grace before SIGKILL"
        );
        assert!(
            done.lines
                .iter()
                .any(|l| l == "signal   SIGKILL pid 301 (reader, user user, unit session-2.scope)")
        );
        assert_eq!(done.signalled.len(), 3);
    }

    /// bd DAS-Backup-Manager-3gzs.14: the lines a clearing says are rows a
    /// reader scans down: a kind word in a fixed column, then the facts.
    #[test]
    fn holder_and_signal_lines_are_aligned_rows() {
        let p = FakeProc::new();
        p.add(Proc {
            pid: 900,
            ppid: 899,
            ..Default::default()
        });
        reader(&p, 5);
        reader(&p, 300);
        let f = Fake {
            proc: &p,
            dies_on: vec![(5, Sig::Term), (300, Sig::Term)],
            sent: RefCell::new(Vec::new()),
        };
        let site = p.site();
        let done = clear_holders(&env(&site, &f, &|_| {}), &das_mount()).unwrap();
        let holder = "reader  reader (fd 3 /run/media/user/das/a (read))  \
                      user user (uid 1000), unit session-2.scope  -> to be stopped";
        assert_eq!(
            done.lines,
            vec![
                format!("holder   pid 5    {holder}"),
                format!("holder   pid 300  {holder}"),
                "signal   SIGTERM pid 5 (reader, user user, unit session-2.scope)".to_string(),
                "signal   SIGTERM pid 300 (reader, user user, unit session-2.scope)".to_string(),
            ]
        );
    }

    /// A writer's row says it is not signalled; the refusal's own text (the
    /// status line's detail) is unchanged.
    #[test]
    fn a_writers_row_says_it_is_not_signalled() {
        let p = FakeProc::new();
        p.add(Proc {
            pid: 301,
            cmd: "cp big /run/media/user/das/",
            fds: vec![(4, "/run/media/user/das/big", "0100001", Some(77))],
            ..Default::default()
        });
        let f = Fake {
            proc: &p,
            dies_on: vec![],
            sent: RefCell::new(Vec::new()),
        };
        let site = p.site();
        let r = clear_holders(&env(&site, &f, &|_| {}), &das_mount()).unwrap_err();
        assert_eq!(r.lines.len(), 1, "{:?}", r.lines);
        assert!(
            r.lines[0].starts_with("holder   pid 301  cp big /run/media/user/das/  writer ("),
            "{:?}",
            r.lines
        );
        assert!(r.lines[0].ends_with("  -> not signalled"), "{:?}", r.lines);
    }

    #[test]
    fn a_writer_is_never_signalled_and_refuses_naming_it() {
        let p = FakeProc::new();
        reader(&p, 300);
        p.add(Proc {
            pid: 301,
            cmd: "cp big /run/media/user/das/",
            fds: vec![(4, "/run/media/user/das/big", "0100001", Some(77))],
            ..Default::default()
        });
        let f = Fake {
            proc: &p,
            dies_on: vec![],
            sent: RefCell::new(Vec::new()),
        };
        let site = p.site();
        let r = clear_holders(&env(&site, &f, &|_| {}), &das_mount()).unwrap_err();
        assert!(
            f.sent.borrow().is_empty(),
            "nothing signalled: {:?}",
            f.sent
        );
        assert!(
            r.why.contains("pid 301") && r.why.contains("writing"),
            "{}",
            r.why
        );
    }

    #[test]
    fn pid1_kernel_threads_and_this_jobs_lineage_are_never_signalled() {
        for (pid, ppid, flags, what) in [
            (1, 0, 0, "PID 1"),
            (2, 0, PF_KTHREAD, "a kernel thread"),
            (899, 1, 0, "ancestors"),
        ] {
            let p = FakeProc::new();
            p.add(Proc {
                pid: 900,
                ppid: 899,
                ..Default::default()
            });
            if pid != 899 {
                p.add(Proc {
                    pid: 899,
                    ppid: 1,
                    ..Default::default()
                });
            }
            p.add(Proc {
                pid,
                ppid,
                flags,
                fds: vec![(3, "/run/media/user/das/a", "0100000", Some(77))],
                ..Default::default()
            });
            reader(&p, 300);
            let f = Fake {
                proc: &p,
                dies_on: vec![],
                sent: RefCell::new(Vec::new()),
            };
            let site = p.site();
            let r = clear_holders(&env(&site, &f, &|_| {}), &das_mount()).unwrap_err();
            assert!(f.sent.borrow().is_empty(), "{what}: {:?}", f.sent);
            assert!(r.why.contains(&format!("pid {pid}")), "{what}: {}", r.why);
        }
    }

    #[test]
    fn a_reused_pid_is_skipped_and_one_turned_writer_refuses_before_sigkill() {
        // 301's pid is reused (a new start time) between the scan and its
        // signal: the new process is never hit.
        let p = FakeProc::new();
        reader(&p, 300);
        reader(&p, 301);
        struct Reuse<'a>(&'a FakeProc, RefCell<Vec<(i32, Sig)>>);
        impl Signaller for Reuse<'_> {
            fn signal(&self, pid: i32, _s: u64, sig: Sig) -> Result<(), String> {
                self.1.borrow_mut().push((pid, sig));
                if pid == 300 {
                    self.0.remove(300);
                    self.0.add(Proc {
                        pid: 301,
                        start: 555,
                        ..Default::default()
                    });
                }
                Ok(())
            }
        }
        let f = Reuse(&p, RefCell::new(Vec::new()));
        let site = p.site();
        let done = clear_holders(&env(&site, &f, &|_| {}), &das_mount()).unwrap();
        assert_eq!(
            *f.1.borrow(),
            vec![(300, Sig::Term)],
            "the reused pid was hit"
        );
        assert!(
            done.lines
                .iter()
                .any(|l| l.starts_with("signal   SIGTERM pid 301 not sent: gone or reused")),
            "{:?}",
            done.lines
        );

        // A reader that opens a file for writing during the grace period.
        let p = FakeProc::new();
        reader(&p, 301);
        struct Turns<'a>(&'a FakeProc, RefCell<Vec<(i32, Sig)>>);
        impl Signaller for Turns<'_> {
            fn signal(&self, pid: i32, _s: u64, sig: Sig) -> Result<(), String> {
                self.1.borrow_mut().push((pid, sig));
                self.0.add(Proc {
                    pid,
                    fds: vec![(4, "/run/media/user/das/new", "0100101", Some(77))],
                    ..Default::default()
                });
                Ok(())
            }
        }
        let f = Turns(&p, RefCell::new(Vec::new()));
        let site = p.site();
        let r = clear_holders(&env(&site, &f, &|_| {}), &das_mount()).unwrap_err();
        assert_eq!(*f.1.borrow(), vec![(301, Sig::Term)], "no SIGKILL");
        assert!(r.why.contains("writing"), "{}", r.why);
    }

    /// btrfs maintenance (balance, replace, device add/remove, resize,
    /// scrub) works by ioctl through a read-only fd: a `btrfs` or `btrbk`
    /// process is a writer, never killed (bd su1b.12 review).
    #[test]
    fn btrfs_and_btrbk_hold_it_as_writers_whatever_their_open_modes() {
        let p = FakeProc::new();
        p.add(Proc {
            pid: 400,
            cmd: "btrfs balance start -dconvert=raid1 /run/media/user/das",
            comm: "btrfs",
            fds: vec![(3, "/run/media/user/das", "0200000", Some(77))],
            ..Default::default()
        });
        p.add(Proc {
            pid: 401,
            cmd: "/usr/bin/perl /usr/bin/btrbk run",
            comm: "btrbk",
            fds: vec![(3, "/run/media/user/das/snap", "0200000", Some(77))],
            ..Default::default()
        });
        let hs = scan_holders(&p.site(), &das_mount()).unwrap();
        let got: Vec<(i32, Mode)> = hs.iter().map(|h| (h.pid, h.mode)).collect();
        assert_eq!(got, vec![(400, Mode::Writer), (401, Mode::Writer)]);
    }

    /// A working directory counts by the mount it is on, never by its path:
    /// a shell in the bare directory under a mount (or a path that only
    /// reads alike in another namespace) is not a holder.
    #[test]
    fn a_working_directory_counts_by_its_mount_never_by_its_path() {
        let real = tempfile::TempDir::new().unwrap();
        let dir = fs::canonicalize(real.path()).unwrap();
        let dir_s = Box::leak(dir.to_string_lossy().into_owned().into_boxed_str());
        let p = FakeProc::new();
        p.add(Proc {
            pid: 500,
            cmd: "bash",
            cwd: dir_s,
            ..Default::default()
        });
        let entry = |id: u64| MountEntry {
            id,
            root: "/".into(),
            mount_point: dir_s.to_string(),
            options: "rw".into(),
            fstype: "btrfs".into(),
            source: "/dev/sdx1".into(),
            dev: "0:52".into(),
            super_options: String::new(),
        };
        let real_id = FakeProc::real_mount_id(&dir);
        let other = scan_holders(&p.site(), &[entry(real_id + 100_000)]).unwrap();
        assert!(
            other.is_empty(),
            "the same path on another mount: {other:?}"
        );
        let on = scan_holders(&p.site(), &[entry(real_id)]).unwrap();
        assert_eq!(on.len(), 1, "{on:?}");
        assert_eq!(on[0].mode, Mode::CwdOnly);
    }

    /// A dry run says whom it would stop, signals nobody, and refuses where
    /// the real run would.
    #[test]
    fn report_only_names_what_it_would_stop_and_signals_nothing() {
        let p = FakeProc::new();
        reader(&p, 300);
        let f = Fake {
            proc: &p,
            dies_on: vec![],
            sent: RefCell::new(Vec::new()),
        };
        let site = p.site();
        let mut e = env(&site, &f, &|_| {});
        e.report_only = true;
        let done = clear_holders(&e, &das_mount()).unwrap();
        assert!(f.sent.borrow().is_empty(), "{:?}", f.sent);
        assert!(done.signalled.is_empty());
        assert_eq!(done.would_stop.len(), 1, "{done:?}");
        assert!(
            done.would_stop[0].starts_with("would stop: pid 300"),
            "{done:?}"
        );
        assert_eq!(done.lines.len(), 1, "{:?}", done.lines);
        assert!(
            done.lines[0].starts_with("holder   pid 300  reader  reader (")
                && done.lines[0].ends_with("  -> would stop"),
            "{:?}",
            done.lines
        );
        p.add(Proc {
            pid: 301,
            fds: vec![(4, "/run/media/user/das/f", "0100002", Some(77))],
            ..Default::default()
        });
        let r = clear_holders(&e, &das_mount()).unwrap_err();
        assert!(r.why.contains("pid 301"), "{}", r.why);
        assert!(f.sent.borrow().is_empty());
    }

    /// A reader that appears during the grace gets SIGTERM and a grace of
    /// its own before SIGKILL.
    #[test]
    fn a_holder_that_appears_during_the_grace_gets_sigterm_before_sigkill() {
        let p = FakeProc::new();
        reader(&p, 300);
        struct Arrives<'a>(&'a FakeProc, RefCell<Vec<(i32, Sig)>>);
        impl Signaller for Arrives<'_> {
            fn signal(&self, pid: i32, _s: u64, sig: Sig) -> Result<(), String> {
                self.1.borrow_mut().push((pid, sig));
                if pid == 300 {
                    self.0.remove(300);
                    reader(self.0, 302);
                }
                if (pid, sig) == (302, Sig::Kill) {
                    self.0.remove(302);
                }
                Ok(())
            }
        }
        let f = Arrives(&p, RefCell::new(Vec::new()));
        let site = p.site();
        clear_holders(&env(&site, &f, &|_| {}), &das_mount()).unwrap();
        assert_eq!(
            *f.1.borrow(),
            vec![(300, Sig::Term), (302, Sig::Term), (302, Sig::Kill)]
        );
    }

    #[test]
    fn a_holder_that_survives_sigkill_refuses() {
        let p = FakeProc::new();
        reader(&p, 300);
        let f = Fake {
            proc: &p,
            dies_on: vec![],
            sent: RefCell::new(Vec::new()),
        };
        let site = p.site();
        let r = clear_holders(&env(&site, &f, &|_| {}), &das_mount()).unwrap_err();
        assert_eq!(*f.sent.borrow(), vec![(300, Sig::Term), (300, Sig::Kill)]);
        assert!(r.why.contains("after SIGTERM and SIGKILL"), "{}", r.why);
    }

    #[test]
    fn nobody_holding_it_is_cleared_with_nothing_sent() {
        let p = FakeProc::new();
        p.add(Proc {
            pid: 400,
            ..Default::default()
        });
        let f = Fake {
            proc: &p,
            dies_on: vec![],
            sent: RefCell::new(Vec::new()),
        };
        let site = p.site();
        let done = clear_holders(&env(&site, &f, &|_| {}), &das_mount()).unwrap();
        assert!(done.lines.is_empty() && f.sent.borrow().is_empty());
    }

    /// The real `/proc`, read only: this test process's own files, one open
    /// for reading and one for writing, are found with the right modes by
    /// their mount id. Nothing is signalled.
    #[test]
    fn this_process_is_found_holding_its_own_files_in_the_real_proc() {
        let dir = tempfile::TempDir::new().unwrap();
        let (r, w) = (dir.path().join("r"), dir.path().join("w"));
        fs::write(&r, "x").unwrap();
        let _ro = fs::File::open(&r).unwrap();
        let _rw = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&w)
            .unwrap();
        let text = fs::read_to_string("/proc/self/mountinfo").unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        let path = path.to_string_lossy();
        let mount = parse_mountinfo(&text)
            .into_iter()
            .filter(|m| under(&path, &m.mount_point))
            .max_by_key(|m| m.mount_point.len())
            .unwrap();
        let me = std::process::id() as i32;
        let hs = scan_holders(&ProcSite::host(), std::slice::from_ref(&mount)).unwrap();
        let h = hs
            .iter()
            .find(|h| h.pid == me)
            .unwrap_or_else(|| panic!("pid {me} not among the holders of {}", mount.mount_point));
        assert_eq!(h.mode, Mode::Writer, "{h}");
        let r = r.to_string_lossy();
        let w = w.to_string_lossy();
        assert!(
            h.uses.iter().any(|u| u.ends_with(&format!("{r} (read)"))),
            "{h}"
        );
        assert!(
            h.uses.iter().any(|u| u.ends_with(&format!("{w} (WRITE)"))),
            "{h}"
        );
    }

    #[test]
    fn the_host_signaller_refuses_in_a_test_build() {
        let e = SystemSignaller
            .signal(std::process::id() as i32 + 1_000_000, 0, Sig::Term)
            .unwrap_err();
        assert!(e.contains("test build"), "{e}");
    }

    // -----------------------------------------------------------------------
    // bd DAS-Backup-Manager-su1b.34: the survivors of the CI mutation run.
    // -----------------------------------------------------------------------

    #[test]
    fn mountinfo_octal_escapes_decode_to_their_bytes_and_only_whole_ones() {
        assert_eq!(unescape("a\\040b"), "a b");
        assert_eq!(unescape("\\040"), " ");
        assert_eq!(unescape("\\134"), "\\");
        assert_eq!(unescape("x\\011y\\012z"), "x\ty\nz");
        assert_eq!(unescape("\\0409"), " 9");
        // A tail too short for three digits, and a non-octal one, stay as
        // written (never an out-of-range read).
        assert_eq!(unescape("a\\04"), "a\\04");
        assert_eq!(unescape("\\04"), "\\04");
        assert_eq!(unescape("a\\0x9"), "a\\0x9");
        // Digits that do not follow a backslash are never decoded.
        assert_eq!(unescape("ab123456"), "ab123456");
    }

    #[test]
    fn a_malformed_mountinfo_line_is_skipped_and_a_spaced_option_list_is_not() {
        let text = "\
77 30 0:52 / /mnt rw - btrfs /dev/sdx1 rw,subvolid=5\n\
77 30 0:52 / /mnt - btrfs /dev/sdx1 rw\n\
78 30 0:52 / /mnt rw - btrfs /dev/sdx1\n\
xx 30 0:52 / /mnt rw - btrfs /dev/sdx1 rw\n\
79 30 0:52 / /mnt rw btrfs /dev/sdx1 rw\n\
80 30 0:52 / /mnt2 rw - btrfs /dev/sdx1 rw,a b\n";
        let got = parse_mountinfo(text);
        let ids: Vec<u64> = got.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![77, 80], "{got:?}");
        assert_eq!(got[1].super_options, "rw,a b");
    }

    #[test]
    fn modes_and_holders_are_shown_in_full() {
        assert_eq!(Mode::CwdOnly.to_string(), "working directory only");
        assert_eq!(Mode::Reader.to_string(), "reading");
        assert_eq!(Mode::Writer.to_string(), "WRITING");
        let mk = |user: Option<&str>, uid: Option<u32>, cmd: &str| Holder {
            pid: 7,
            start_time: 1,
            uid,
            user: user.map(str::to_string),
            unit: Some("app.scope".into()),
            cmdline: cmd.into(),
            mode: Mode::Reader,
            kernel_thread: false,
            uses: vec!["fd 3 /x (read)".into(), "cwd /x".into()],
        };
        let line = |u, i, c| mk(u, i, c).to_string();
        assert_eq!(
            line(Some("bosco"), Some(1000), "dolphin"),
            "pid 7 [reading] user bosco (uid 1000), unit app.scope: dolphin — fd 3 /x (read); cwd /x"
        );
        assert!(
            line(None, Some(1000), "dolphin").contains("user uid 1000, unit"),
            "{}",
            line(None, Some(1000), "dolphin")
        );
        assert!(line(None, None, "d").contains("user uid unknown, unit"));
        assert!(line(Some("x"), None, "d").contains("user uid unknown, unit"));
        assert!(line(None, None, "").contains(": (no command line) — "));
        let mut h = mk(None, None, "d");
        h.unit = None;
        assert!(h.to_string().contains("unit none:"), "{h}");
    }

    /// A `/proc/<pid>/stat` with `rest` after the command name.
    fn stat_root(pid: i32, rest: &str) -> tempfile::TempDir {
        let t = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(t.path().join(pid.to_string())).unwrap();
        fs::write(
            t.path().join(pid.to_string()).join("stat"),
            format!("{pid} (a) b) {rest}\n"),
        )
        .unwrap();
        t
    }

    #[test]
    fn a_stat_needs_exactly_twenty_fields_after_the_name_and_no_fewer() {
        // state ppid, then 4 zeros, flags (field 6), then zeros to field 19.
        let twenty = "S 41 0 0 0 0 4194304 0 0 0 0 0 0 0 0 0 0 0 0 777";
        assert_eq!(twenty.split_whitespace().count(), 20);
        let t = stat_root(5, twenty);
        assert_eq!(read_stat(t.path(), 5).unwrap(), ('S', 41, 4_194_304, 777));
        let nineteen = "S 41 0 0 0 0 4194304 0 0 0 0 0 0 0 0 0 0 0 777";
        assert_eq!(nineteen.split_whitespace().count(), 19);
        let t = stat_root(5, nineteen);
        let e = read_stat(t.path(), 5).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        // No closing parenthesis at all.
        let t = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(t.path().join("5")).unwrap();
        fs::write(t.path().join("5/stat"), "5 a S 1\n").unwrap();
        assert!(read_stat(t.path(), 5).is_err());
    }

    #[test]
    fn start_time_is_none_for_a_zombie_a_dead_one_and_a_missing_one() {
        let p = FakeProc::new();
        for (pid, state) in [(10, 'S'), (11, 'Z'), (12, 'X'), (13, 'R')] {
            p.add(Proc {
                pid,
                state,
                start: 4242,
                ..Default::default()
            });
        }
        let root = p.site().proc_root;
        assert_eq!(start_time(&root, 10), Some(4242));
        assert_eq!(start_time(&root, 13), Some(4242));
        assert_eq!(start_time(&root, 11), None, "zombie");
        assert_eq!(start_time(&root, 12), None, "dead");
        assert_eq!(start_time(&root, 99), None, "missing");
    }

    #[test]
    fn a_lineage_stops_at_pid_1_and_never_holds_pid_0() {
        let p = FakeProc::new();
        p.add(Proc {
            pid: 1,
            ppid: 0,
            ..Default::default()
        });
        p.add(Proc {
            pid: 899,
            ppid: 1,
            ..Default::default()
        });
        p.add(Proc {
            pid: 900,
            ppid: 899,
            ..Default::default()
        });
        let root = p.site().proc_root;
        assert_eq!(lineage(&root, 900), BTreeSet::from([1, 899, 900]));
        // A pid with no stat is itself and nothing more.
        assert_eq!(lineage(&root, 555), BTreeSet::from([555]));
    }

    #[test]
    fn the_mount_id_is_only_taken_when_the_kernel_says_it_filled_it_in() {
        assert_eq!(reported_mnt_id(libc::STATX_MNT_ID, 5), Some(5));
        assert_eq!(
            reported_mnt_id(libc::STATX_MNT_ID | libc::STATX_BASIC_STATS, 5),
            Some(5)
        );
        assert_eq!(reported_mnt_id(0, 5), None);
        assert_eq!(reported_mnt_id(libc::STATX_BASIC_STATS, 5), None);
    }

    #[test]
    fn only_a_vanished_file_or_process_is_gone() {
        assert!(is_gone(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(is_gone(&io::Error::from_raw_os_error(libc::ESRCH)));
        assert!(is_gone(&io::Error::from_raw_os_error(libc::ENOENT)));
        assert!(!is_gone(&io::Error::from_raw_os_error(libc::EACCES)));
        assert!(!is_gone(&io::Error::from_raw_os_error(libc::EIO)));
        assert!(!is_gone(&io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(!is_gone(&io::Error::other("x")));
    }

    #[test]
    fn a_user_is_looked_up_by_uid_and_a_short_passwd_line_is_no_match() {
        let dir = tempfile::TempDir::new().unwrap();
        let pw = dir.path().join("passwd");
        fs::write(
            &pw,
            "root:x:0:0::/root:/bin/sh\nshort:x\nbob:x:1001:1001::/h:/bin/sh\nbad:x:nan:1\n",
        )
        .unwrap();
        assert_eq!(user_name(&pw, 1001).as_deref(), Some("bob"));
        assert_eq!(user_name(&pw, 0).as_deref(), Some("root"));
        assert_eq!(user_name(&pw, 5), None);
        assert_eq!(user_name(&dir.path().join("absent"), 0), None);
    }

    #[test]
    fn the_unit_is_the_deepest_named_cgroup_component_else_the_whole_path() {
        let u = |c: &str| unit_of(c);
        assert_eq!(
            u("0::/user.slice/user-1000.slice/session-2.scope\n").as_deref(),
            Some("session-2.scope")
        );
        // The deepest component with a unit suffix, not the deepest.
        assert_eq!(
            u("0::/system.slice/foo.service/sub\n").as_deref(),
            Some("foo.service")
        );
        // A slice alone is not a unit: the path is shown whole.
        assert_eq!(u("0::/system.slice\n").as_deref(), Some("/system.slice"));
        assert_eq!(
            u("0::/a.slice/b.slice\n").as_deref(),
            Some("/a.slice/b.slice")
        );
        // cgroup v1 lines.
        assert_eq!(
            u("12:name=systemd:/x/foo.service\n1:cpu:/\n").as_deref(),
            Some("foo.service")
        );
        // Nothing to show.
        assert_eq!(u("0::\n"), None);
        assert_eq!(u(""), None);
    }

    #[test]
    fn a_unit_with_its_own_file_is_shown_by_that_file_not_as_generated() {
        let both = "Id=mnt-x.mount\nLoadState=loaded\nFragmentPath=/etc/systemd/system/mnt-x.mount\nSourcePath=/etc/fstab\n";
        assert_eq!(
            origin("/mnt/x", Some(both)),
            Origin::Unit {
                unit: "mnt-x.mount".into(),
                from: "/etc/systemd/system/mnt-x.mount".into()
            }
        );
        assert_eq!(
            origin("/mnt/x", Some("Id=mnt-x.mount\nLoadState=not-found\n")),
            Origin::Other
        );
        assert_eq!(Origin::Other.to_string(), "a hand mount or another program");
    }

    // --- scan_holders: what is read, what is left out ----------------------

    fn scanned(p: &FakeProc, mounts: &[MountEntry]) -> Vec<(i32, Mode)> {
        scan_holders(&p.site(), mounts)
            .unwrap()
            .iter()
            .map(|h| (h.pid, h.mode))
            .collect()
    }

    fn mount_at(id: u64, point: &str) -> MountEntry {
        MountEntry {
            id,
            root: "/".into(),
            mount_point: point.into(),
            options: "rw".into(),
            fstype: "btrfs".into(),
            source: "/dev/sdx1".into(),
            dev: "0:52".into(),
            super_options: String::new(),
        }
    }

    /// Gives a directory its permissions back when it goes out of scope, so
    /// the temporary tree can be removed.
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    fn unreadable(path: PathBuf) -> Restore {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        Restore(path)
    }

    fn running_as_root() -> bool {
        // SAFETY: geteuid(2) takes nothing and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn a_process_whose_open_files_or_maps_vanish_is_left_out() {
        let p = FakeProc::new();
        for pid in [700, 701] {
            p.add(Proc {
                pid,
                ..Default::default()
            });
        }
        let root = p.site().proc_root;
        fs::remove_dir_all(root.join("700/fd")).unwrap();
        fs::remove_file(root.join("701/maps")).unwrap();
        assert_eq!(scanned(&p, &das_mount()), vec![]);
    }

    /// What cannot be read is never assumed harmless: a writer, listed. (A
    /// directory's mode does not stop root, so this runs unprivileged only.)
    #[test]
    fn a_process_whose_open_files_or_maps_cannot_be_read_is_a_writer() {
        if running_as_root() {
            return;
        }
        let p = FakeProc::new();
        for pid in [702, 703] {
            p.add(Proc {
                pid,
                ..Default::default()
            });
        }
        let root = p.site().proc_root;
        let _a = unreadable(root.join("702/fd"));
        let _b = unreadable(root.join("703/maps"));
        let hs = scan_holders(&p.site(), &das_mount()).unwrap();
        let got: Vec<(i32, Mode)> = hs.iter().map(|h| (h.pid, h.mode)).collect();
        assert_eq!(got, vec![(702, Mode::Writer), (703, Mode::Writer)]);
        assert!(
            hs[0].uses[0].contains("open files cannot be read"),
            "{:?}",
            hs[0]
        );
        assert!(
            hs[1].uses[0].contains("mappings cannot be read"),
            "{:?}",
            hs[1]
        );
    }

    /// A working directory that is gone is not a holder; one whose mount
    /// cannot be told, under the mount's path, is a writer.
    #[test]
    fn a_vanished_working_directory_is_no_holder_and_an_unreadable_one_is_a_writer() {
        let p = FakeProc::new();
        // The link names a directory that no longer exists.
        p.add(Proc {
            pid: 710,
            cwd: "/run/media/user/das/deleted-dir",
            ..Default::default()
        });
        // The link loops: statx fails with ELOOP, which is not "gone".
        let real = tempfile::TempDir::new().unwrap();
        let dir = fs::canonicalize(real.path()).unwrap();
        let looping = dir.join("loop");
        symlink(&looping, &looping).unwrap();
        let looping: &'static str =
            Box::leak(looping.to_string_lossy().into_owned().into_boxed_str());
        p.add(Proc {
            pid: 711,
            cwd: looping,
            ..Default::default()
        });
        let mut mounts = das_mount();
        mounts.push(mount_at(4242, &dir.to_string_lossy()));
        let hs = scan_holders(&p.site(), &mounts).unwrap();
        let got: Vec<(i32, Mode)> = hs.iter().map(|h| (h.pid, h.mode)).collect();
        assert_eq!(got, vec![(711, Mode::Writer)], "{hs:?}");
        assert!(
            hs[0].uses[0].contains("its mount cannot be told"),
            "{:?}",
            hs[0]
        );
    }

    #[test]
    fn mappings_are_classified_by_their_permissions_and_only_under_the_mount() {
        let map = |perms: &str, path: &str| format!("7f00-7f10 {perms} 00000000 00:34 12 {path}\n");
        let p = FakeProc::new();
        let on = "/run/media/user/das/lib.so";
        let cases = [
            (610, map("r-xp", on)),
            (611, map("rw-p", on)),
            (612, map("r--s", on)),
            (613, map("rw-s", on)),
            // Nothing there: an anonymous mapping (five fields), the heap,
            // and a file elsewhere.
            (
                614,
                format!(
                    "7f00-7f10 rw-p 00000000 00:00 0\n55d0-55e0 rw-p 00000000 00:00 0      [heap]\n{}",
                    map("r-xp", "/usr/lib/libc.so.6")
                ),
            ),
        ];
        for (pid, maps) in &cases {
            p.add(Proc {
                pid: *pid,
                maps: Box::leak(maps.clone().into_boxed_str()),
                ..Default::default()
            });
        }
        assert_eq!(
            scanned(&p, &das_mount()),
            vec![
                (610, Mode::Reader),
                (611, Mode::Reader),
                (612, Mode::Reader),
                (613, Mode::Writer),
            ]
        );
    }

    /// btrfs maintenance is recognised by the first word of its command line
    /// as well as by its name.
    #[test]
    fn a_btrfs_command_line_alone_makes_a_holder_a_writer() {
        let p = FakeProc::new();
        p.add(Proc {
            pid: 620,
            cmd: "/usr/bin/btrfs balance start /run/media/user/das",
            comm: "x",
            fds: vec![(3, "/run/media/user/das", "0200000", Some(77))],
            ..Default::default()
        });
        p.add(Proc {
            pid: 621,
            cmd: "/usr/bin/btrfs-helper run",
            comm: "x",
            fds: vec![(3, "/run/media/user/das", "0200000", Some(77))],
            ..Default::default()
        });
        assert_eq!(
            scanned(&p, &das_mount()),
            vec![(620, Mode::Writer), (621, Mode::Reader)]
        );
    }

    /// A pid reused by another process during the grace is a new holder: it
    /// gets SIGTERM and a grace of its own, never an immediate SIGKILL.
    #[test]
    fn a_reused_pid_during_the_grace_gets_sigterm_not_sigkill() {
        let p = FakeProc::new();
        reader(&p, 300);
        struct Reuses<'a>(&'a FakeProc, RefCell<Vec<(i32, Sig)>>);
        impl Signaller for Reuses<'_> {
            fn signal(&self, pid: i32, _s: u64, sig: Sig) -> Result<(), String> {
                let first = self.1.borrow().is_empty();
                self.1.borrow_mut().push((pid, sig));
                self.0.remove(pid);
                if first {
                    // Another process takes the pid, a reader too.
                    self.0.add(Proc {
                        pid,
                        start: 555,
                        cmd: "other",
                        fds: vec![(3, "/run/media/user/das/b", "0100000", Some(77))],
                        ..Default::default()
                    });
                }
                Ok(())
            }
        }
        let f = Reuses(&p, RefCell::new(Vec::new()));
        let site = p.site();
        clear_holders(&env(&site, &f, &|_| {}), &das_mount()).unwrap();
        assert_eq!(
            *f.1.borrow(),
            vec![(300, Sig::Term), (300, Sig::Term)],
            "the reused pid went straight to SIGKILL"
        );
    }

    // --- the host signaller, on processes this test spawned ----------------

    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn spawn_sleeper() -> Child {
        Child(
            std::process::Command::new("sleep")
                .arg("60")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn exited(c: &mut Child) -> Option<std::process::ExitStatus> {
        for _ in 0..100 {
            if let Some(s) = c.0.try_wait().unwrap() {
                return Some(s);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    fn real_start(pid: i32) -> u64 {
        start_time(Path::new("/proc"), pid).expect("a live child")
    }

    #[test]
    fn signal_process_ends_a_child_it_was_given_with_the_signal_asked_for() {
        use std::os::unix::process::ExitStatusExt;
        for (sig, signo) in [(Sig::Term, libc::SIGTERM), (Sig::Kill, libc::SIGKILL)] {
            let mut c = spawn_sleeper();
            let pid = c.0.id() as i32;
            let start = real_start(pid);
            assert_eq!(signal_process(Path::new("/proc"), pid, start, sig), Ok(()));
            let status = exited(&mut c).expect("the child ended");
            assert_eq!(status.signal(), Some(signo), "{sig:?}: {status:?}");
        }
    }

    #[test]
    fn signal_process_refuses_a_pid_whose_start_time_is_not_the_one_checked() {
        let mut c = spawn_sleeper();
        let pid = c.0.id() as i32;
        let start = real_start(pid);
        let e = signal_process(Path::new("/proc"), pid, start + 1, Sig::Kill).unwrap_err();
        assert!(e.contains("no longer the process"), "{e}");
        std::thread::sleep(Duration::from_millis(200));
        assert!(c.0.try_wait().unwrap().is_none(), "the child was signalled");
    }

    #[test]
    fn signal_process_reports_a_pid_that_is_gone() {
        let mut c = spawn_sleeper();
        let pid = c.0.id() as i32;
        let start = real_start(pid);
        c.0.kill().unwrap();
        c.0.wait().unwrap();
        let e = signal_process(Path::new("/proc"), pid, start, Sig::Term).unwrap_err();
        assert!(e.starts_with(&format!("pidfd_open({pid}): ")), "{e}");
    }

    /// Never a signal to PID 1, to 0 (a process group) or to a negative pid
    /// (a group, or every process): refused before any syscall. The start
    /// time given matches nothing, so even a check that failed to refuse
    /// would not signal.
    #[test]
    fn signal_process_never_signals_pid_1_or_less() {
        for pid in [1, 0, -1, -4242] {
            let e = signal_process(Path::new("/proc"), pid, u64::MAX, Sig::Kill).unwrap_err();
            assert_eq!(e, format!("refused: never a signal to pid {pid}"));
        }
    }
}
