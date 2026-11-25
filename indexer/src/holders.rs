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
    let mut names: BTreeSet<String> = BTreeSet::new();
    for p in member_paths {
        names.insert(p.clone());
        if let Ok(c) = fs::canonicalize(p) {
            names.insert(c.to_string_lossy().into_owned());
        }
    }
    let mut out: Vec<MountEntry> = entries
        .iter()
        .filter(|e| {
            e.fstype == "btrfs"
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
    Ok((buf.stx_mask & libc::STATX_MNT_ID != 0).then_some(buf.stx_mnt_id))
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
            .map(|b| {
                b.split(|c| *c == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
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
        if pid <= 1 {
            return Err(format!("refused: never a signal to pid {pid}"));
        }
        // SAFETY: plain syscalls on integer arguments; the fd is closed below.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(format!("pidfd_open({pid}): {}", io::Error::last_os_error()));
        }
        let fd = fd as libc::c_int;
        let result = if start_time(Path::new("/proc"), pid) != Some(expected) {
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
}

/// What [`clear_holders`] did: one line per holder seen and per signal sent,
/// for the job's log and report.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cleared {
    pub lines: Vec<String>,
    /// The signals sent: (pid, signal, command line).
    pub signalled: Vec<(i32, Sig, String)>,
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
    for h in &holders {
        done.lines.push(format!("holder: {h}"));
    }
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
    if env.report_only {
        done.would_stop = holders.iter().map(|h| format!("would stop: {h}")).collect();
        done.lines.extend(done.would_stop.iter().cloned());
        return Ok(done);
    }
    let signal_all = |hs: &[Holder], sig: Sig, done: &mut Cleared| {
        for h in hs {
            if start_time(&env.site.proc_root, h.pid) != Some(h.start_time) {
                done.lines.push(format!(
                    "pid {}: gone or reused before {} — not signalled",
                    h.pid,
                    sig.name()
                ));
                continue;
            }
            match env.signaller.signal(h.pid, h.start_time, sig) {
                Ok(()) => {
                    done.lines.push(format!(
                        "sent {} to pid {} ({})",
                        sig.name(),
                        h.pid,
                        h.cmdline
                    ));
                    done.signalled.push((h.pid, sig, h.cmdline.clone()));
                }
                Err(e) => done
                    .lines
                    .push(format!("pid {}: {} not sent: {e}", h.pid, sig.name())),
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
        for h in &left {
            done.lines.push(format!("still a holder: {h}"));
        }
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
        for h in &newcomers {
            done.lines.push(format!("new holder: {h}"));
        }
        signal_all(&newcomers, Sig::Term, &mut done);
        let again = wait(TERM_GRACE, &done)?;
        if let Some(why) = check(&again) {
            for h in &again {
                done.lines.push(format!("still a holder: {h}"));
            }
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
    for h in &left {
        done.lines.push(format!("still a holder: {h}"));
    }
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

#[cfg(test)]
mod tests {
    use super::testproc::{FakeProc, Proc};
    use super::*;
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
                .any(|l| l == "sent SIGKILL to pid 301 (reader)")
        );
        assert_eq!(done.signalled.len(), 3);
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
                .any(|l| l.contains("pid 301: gone or reused")),
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
}
