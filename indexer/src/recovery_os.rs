//! The independent operating systems on the recovery drives, and how far they
//! have fallen behind the host (bd DAS-Backup-Manager-xd3).
//!
//! Each `role = "mirror"` target carries its own bootable install under `@` on
//! the same filesystem that receives the backups. It is the system to boot
//! when the host cannot, so it must be able to mount and `btrfs receive` what
//! the host's current kernel and btrfs-progs wrote — and it must still be able
//! to update itself (an old keyring cannot). This module reads that install
//! **read-only** and says when it is stale. It never writes under the root it
//! inspects — not even an access time: every open is `O_NOATIME`, and no
//! symlink inside the root is followed, because following one updates the
//! link's atime (and a link to an absolute path would report the HOST's files
//! as the recovery OS's). Both were seen moving the `@` subvolume's generation
//! on a real filesystem before they were closed.
//!
//! An item that cannot be read is `None` plus a `problems` entry, and an
//! unknown upgrade date or kernel is stale — never a fabricated age.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::caldate::{date_of, day_number};
use crate::config::{Config, TargetRole};
use crate::fsutil::CommandRunner;

/// Where `btrdasd recovery-os status --state-file` keeps the last reading of
/// each drive, so `btrdasd health` can show it between runs.
pub const RECOVERY_OS_STATE_PATH: &str = "/var/lib/das-backup/recovery-os.json";

/// The packages whose installed versions are read from the recovery OS.
pub const WATCHED_PACKAGES: [&str; 4] = [
    "btrfs-progs",
    "btrbk",
    "linux-cachyos",
    "das-backup-manager",
];

const UPGRADE_MARKER: &str = "starting full system upgrade";
const OS_RELEASE: &str = "etc/os-release";
/// Where `os-release(5)` says to look when `etc/os-release` is absent; on
/// Arch-family systems `etc/os-release` is a symlink to it, which is not
/// followed (see `resolve_in_root`).
const OS_RELEASE_FALLBACK: &str = "usr/lib/os-release";
const MODULES: &str = "usr/lib/modules";
const PACMAN_LOG: &str = "var/log/pacman.log";
const PACMAN_LOCAL: &str = "var/lib/pacman/local";

/// What was read from one recovery OS root.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RecoveryOs {
    /// `PRETTY_NAME`, else `NAME`, from `etc/os-release`.
    pub os_name: Option<String>,
    /// `YYYY-MM-DD` of the last full system upgrade that was APPLIED: a
    /// `starting full system upgrade` line in pacman's log followed by
    /// `[ALPM] transaction completed` before the next invocation starts.
    pub last_full_upgrade_applied: Option<String>,
    /// `YYYY-MM-DD` of the last `starting full system upgrade` line, applied
    /// or not. pacman logs it before resolving, downloading or checking
    /// signatures, so an upgrade that failed (an expired keyring) or was
    /// answered "n" still stamps it.
    pub last_full_upgrade_attempted: Option<String>,
    /// Whether that last attempt is the applied one.
    pub last_attempt_completed: bool,
    /// `YYYY-MM-DD` of the first line of pacman's log: when the OS was
    /// installed (pacman's first entry is the `pacman -b /mnt/...` of the
    /// install). `None` when the log was not read or its first line carries
    /// no date. It is a log's age, so a rotated log would make it the date
    /// of the rotation; pacman's log is not rotated on these installs.
    pub installed: Option<String>,
    /// Whether `var/log/pacman.log` was read. When false, `problems` says why
    /// if it was unreadable; with no problem for it, it is absent.
    pub log_read: bool,
    /// Whether `usr/lib/modules` was listed, with the same absent/unreadable
    /// split as `log_read`.
    pub modules_read: bool,
    /// Directory names under `usr/lib/modules`, sorted.
    pub kernels: Vec<String>,
    /// Name → version for the [`WATCHED_PACKAGES`] that are installed.
    pub packages: BTreeMap<String, String>,
    /// Whether pacman's database could be listed. When false, a package
    /// missing from `packages` is unknown, not "not installed".
    pub packages_read: bool,
    /// Every item that is there but could not be read, as
    /// `<path relative to root>: <why>`. An absent item is not a problem: it
    /// is recorded by the `*_read` flags and the `None`s alone.
    pub problems: Vec<String>,
}

/// Why an item could not be read: it is not there, or it is there and
/// could not be read (permissions, I/O, a symlink, a FIFO).
#[derive(Debug, Clone, PartialEq)]
enum ReadErr {
    Absent,
    Unreadable(String),
}

/// `rel` under `root`, refusing every symlink on the way. Following a link
/// updates the link's own access time — a write no open flag prevents — and
/// refusing links also means no path can lead out of the root. Each component
/// is checked with `lstat`, which writes nothing; a `..` or absolute component
/// is refused. A component that does not exist makes the item absent.
fn resolve_in_root(root: &Path, rel: &str) -> Result<PathBuf, ReadErr> {
    let mut path = root.to_path_buf();
    for component in Path::new(rel).components() {
        let Component::Normal(name) = component else {
            return Err(ReadErr::Unreadable(format!(
                "{rel}: not a plain relative path"
            )));
        };
        path.push(name);
        let meta = fs::symlink_metadata(&path).map_err(|e| io_error(rel, &e))?;
        if meta.file_type().is_symlink() {
            return Err(ReadErr::Unreadable(format!(
                "{rel}: {} is a symlink, not followed",
                path.display()
            )));
        }
    }
    Ok(path)
}

/// `NotFound` is absent; anything else is unreadable, said by [`open_error`].
fn io_error(rel: &str, e: &std::io::Error) -> ReadErr {
    if e.kind() == std::io::ErrorKind::NotFound {
        ReadErr::Absent
    } else {
        ReadErr::Unreadable(open_error(rel, e))
    }
}

/// Why an open failed, said the way a reader can act on. `EPERM` from
/// `O_NOATIME` means "not the owner and not root": the read would have had to
/// update access times — a write to the recovery OS — so it is refused.
fn open_error(rel: &str, e: &std::io::Error) -> String {
    if e.raw_os_error() == Some(libc::EPERM) {
        format!("{rel}: cannot be read without updating its access time (run as root)")
    } else {
        format!("{rel}: {e}")
    }
}

/// Open flags for a file: no access-time update, and never through a symlink
/// (defence in depth behind `resolve_in_root`, against a link swapped in
/// after its check).
///
/// The flags are single, distinct bits, so `+` is `|` here (asserted below).
/// It is written `+` because a mutation test cannot tell `|` from `^` on
/// disjoint bits — the same number — while every mutation of `+` changes it.
const FILE_FLAGS: i32 = libc::O_NOATIME + libc::O_NOFOLLOW;
/// The same for a directory, which must be one.
const DIR_FLAGS: i32 = libc::O_NOATIME + libc::O_NOFOLLOW + libc::O_DIRECTORY;
const _: () = assert!(
    libc::O_NOATIME & libc::O_NOFOLLOW == 0
        && libc::O_NOATIME & libc::O_DIRECTORY == 0
        && libc::O_NOFOLLOW & libc::O_DIRECTORY == 0
);

/// Open read-only with `flags` ([`FILE_FLAGS`] or [`DIR_FLAGS`]). Every read
/// here goes through this: an atime update is a metadata write that moves
/// the recovery OS subvolume's generation.
fn open_noatime(path: &Path, flags: i32) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
}

/// Read a regular file under `root`. A FIFO or device in its place is refused
/// before it is opened, so a read can never block.
fn read_in_root(root: &Path, rel: &str) -> Result<String, ReadErr> {
    let path = resolve_in_root(root, rel)?;
    let meta = fs::metadata(&path).map_err(|e| io_error(rel, &e))?;
    if !meta.is_file() {
        return Err(ReadErr::Unreadable(format!("{rel}: not a regular file")));
    }
    let mut bytes = Vec::new();
    open_noatime(&path, FILE_FLAGS)
        .map_err(|e| io_error(rel, &e))?
        .read_to_end(&mut bytes)
        .map_err(|e| ReadErr::Unreadable(format!("{rel}: {e}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The entry names of a directory, read through a descriptor opened with
/// `O_NOATIME` (`std::fs::read_dir` takes no open flags, and `readdir`
/// updates a directory's atime like `read` does a file's).
fn dir_names_noatime(path: &Path) -> std::io::Result<Vec<String>> {
    use std::os::fd::IntoRawFd;
    let fd = open_noatime(path, DIR_FLAGS)?.into_raw_fd();
    // SAFETY: `fd` is an open directory descriptor this function owns; on
    // success `fdopendir` takes ownership of it and `closedir` releases both.
    let dirp = unsafe { libc::fdopendir(fd) };
    if dirp.is_null() {
        let e = std::io::Error::last_os_error();
        // SAFETY: fdopendir failed, so `fd` is still ours to close.
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let mut names = Vec::new();
    let result = loop {
        // SAFETY: errno is thread-local; clearing it is the only way to tell
        // the end of the directory from an error when readdir returns NULL.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: `dirp` is a valid, open DIR stream until closedir below.
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            let e = std::io::Error::last_os_error();
            break if e.raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(e)
            };
        }
        // SAFETY: `ent` is non-null and points at a dirent whose d_name is
        // NUL-terminated, valid until the next readdir on this stream.
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        let name = name.to_string_lossy();
        if name != "." && name != ".." {
            names.push(name.into_owned());
        }
    };
    // SAFETY: `dirp` came from fdopendir and is closed exactly once.
    unsafe { libc::closedir(dirp) };
    result.map(|()| names)
}

/// The names of the subdirectories of a directory under `root`, sorted.
fn list_in_root(root: &Path, rel: &str) -> Result<Vec<String>, ReadErr> {
    let path = resolve_in_root(root, rel)?;
    let mut names: Vec<String> = dir_names_noatime(&path)
        .map_err(|e| io_error(rel, &e))?
        .into_iter()
        // lstat: a stat would follow a symlinked entry and touch its atime.
        .filter(|n| fs::symlink_metadata(path.join(n)).is_ok_and(|m| m.is_dir()))
        .collect();
    names.sort();
    Ok(names)
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    v
}

fn parse_os_release(text: &str) -> Option<String> {
    let field = |key: &str| {
        text.lines()
            .filter_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(unquote)
            .find(|v| !v.is_empty())
            .map(String::from)
    };
    field("PRETTY_NAME").or_else(|| field("NAME"))
}

/// What pacman's log says about full system upgrades.
#[derive(Debug, Default, PartialEq)]
struct Upgrades {
    installed: Option<String>,
    applied: Option<String>,
    attempted: Option<String>,
    last_completed: bool,
}

/// The `YYYY-MM-DD` a pacman log line is stamped with: `[YYYY-MM-DDTHH:…]`
/// (older logs `[YYYY-MM-DD HH:MM]`), or `None`.
fn line_date(line: &str) -> Option<String> {
    let date = line.strip_prefix('[')?.get(..10)?;
    day_number(date).map(|_| date.to_string())
}

/// The install date is the first line's: pacman's first entry on an install
/// is its `pacman -b /mnt/...` from the live ISO. A first line without a date
/// gives none; a later line's date is never taken for it.
///
/// An attempt is a `starting full system upgrade` line; it was applied if
/// `[ALPM] transaction completed` follows before the next invocation starts
/// (pacman logs `Running '…'` at the start of every invocation) or the next
/// `starting` line. A marker whose date does not parse is never taken, and
/// closes the window so a later completion is not credited to an earlier one.
fn parse_upgrades(log: &str) -> Upgrades {
    let mut u = Upgrades {
        installed: log.lines().next().and_then(line_date),
        ..Upgrades::default()
    };
    let mut open: Option<String> = None;
    for line in log.lines() {
        if line.contains(UPGRADE_MARKER) {
            open = line_date(line);
            if let Some(date) = &open {
                u.attempted = Some(date.clone());
                u.last_completed = false;
            }
        } else if line.contains("[PACMAN] Running ") || line.contains("[PACMAN] starting ") {
            open = None;
        } else if line.contains("[ALPM] transaction completed")
            && let Some(date) = open.take()
        {
            u.applied = Some(date);
            u.last_completed = true;
        }
    }
    u
}

/// `(%NAME%, %VERSION%)` from a pacman `desc` file.
fn parse_desc(text: &str) -> Option<(String, String)> {
    let field = |key: &str| {
        let mut lines = text.lines();
        lines.find(|l| *l == key)?;
        lines.next().filter(|v| !v.is_empty()).map(String::from)
    };
    Some((field("%NAME%")?, field("%VERSION%")?))
}

/// Read a recovery OS root (`<mount>/@`). Pure reads; see the module doc.
/// An absent item is left `None`/unread; an unreadable one is also listed in
/// `problems`.
pub fn inspect(root: &Path) -> RecoveryOs {
    let mut os = RecoveryOs::default();
    let unreadable = |e: ReadErr, problems: &mut Vec<String>| {
        if let ReadErr::Unreadable(why) = e {
            problems.push(why);
        }
    };
    match read_in_root(root, OS_RELEASE)
        .or_else(|first| read_in_root(root, OS_RELEASE_FALLBACK).map_err(|second| [first, second]))
    {
        Ok(text) => os.os_name = parse_os_release(&text),
        Err(both) => both
            .into_iter()
            .for_each(|e| unreadable(e, &mut os.problems)),
    }
    match list_in_root(root, MODULES) {
        Ok(names) => {
            os.modules_read = true;
            os.kernels = names;
        }
        Err(e) => unreadable(e, &mut os.problems),
    }
    match read_in_root(root, PACMAN_LOG) {
        Ok(text) => {
            let u = parse_upgrades(&text);
            os.log_read = true;
            os.installed = u.installed;
            os.last_full_upgrade_applied = u.applied;
            os.last_full_upgrade_attempted = u.attempted;
            os.last_attempt_completed = u.last_completed;
        }
        Err(e) => unreadable(e, &mut os.problems),
    }
    match list_in_root(root, PACMAN_LOCAL) {
        Ok(entries) => {
            os.packages_read = true;
            for entry in entries {
                let rel = format!("{PACMAN_LOCAL}/{entry}/desc");
                match read_in_root(root, &rel) {
                    Ok(text) => {
                        if let Some((name, version)) = parse_desc(&text)
                            && WATCHED_PACKAGES.contains(&name.as_str())
                        {
                            os.packages.insert(name, version);
                        }
                    }
                    // An entry without its desc is a damaged database, not
                    // an absent package: the entry might be a watched one.
                    Err(ReadErr::Absent) => os.problems.push(format!("{rel}: absent")),
                    Err(ReadErr::Unreadable(why)) => os.problems.push(why),
                }
            }
        }
        Err(e) => unreadable(e, &mut os.problems),
    }
    os
}

/// The leading numeric `major.minor[.patch]` of a kernel release, or `None`.
fn kernel_numbers(version: &str) -> Option<(u64, u64, u64)> {
    let head: String = version
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = head.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// `(major, minor)` of a kernel release such as `6.12.1-1-cachyos`.
pub fn kernel_series(version: &str) -> Option<(u64, u64)> {
    kernel_numbers(version).map(|(major, minor, _)| (major, minor))
}

/// The highest-numbered kernel; names that are not a version are ignored.
pub fn newest_kernel(kernels: &[String]) -> Option<&String> {
    kernels
        .iter()
        .filter_map(|k| kernel_numbers(k).map(|n| (n, k)))
        .max_by_key(|(n, _)| *n)
        .map(|(_, k)| k)
}

/// The host's running kernel and installed btrfs-progs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostVersions {
    pub kernel: Option<String>,
    pub btrfs_progs: Option<String>,
}

fn stdout_of(runner: &dyn CommandRunner, cmd: &mut Command) -> Option<String> {
    let out = runner.output(cmd).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `uname -r` and `pacman -Q btrfs-progs`, through `runner`.
pub fn host_versions_with(runner: &dyn CommandRunner) -> HostVersions {
    let kernel = stdout_of(runner, Command::new("uname").arg("-r")).filter(|k| !k.is_empty());
    let btrfs_progs = stdout_of(runner, Command::new("pacman").args(["-Q", "btrfs-progs"]))
        .and_then(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some("btrfs-progs"))
                .then(|| fields.next().map(String::from))
                .flatten()
        });
    HostVersions {
        kernel,
        btrfs_progs,
    }
}

/// [`host_versions_with`] on this host.
pub fn host_versions() -> HostVersions {
    host_versions_with(&crate::fsutil::SystemRunner)
}

/// The upstream part of a pacman version (`6.10` of `6.10-1`) as numbers,
/// or `None` when it is not purely dotted numerics (an epoch, `rc`, a git
/// suffix). pacman's own `vercmp` handles those; this does not try to.
fn dotted_numbers(version: &str) -> Option<Vec<u64>> {
    let upstream = version.split_once('-').map_or(version, |(u, _)| u);
    upstream.split('.').map(|p| p.parse().ok()).collect()
}

/// Whether the recovery OS's btrfs-progs is older than the host's: `None`
/// (no verdict) unless both are known and both are dotted numerics. The
/// pkgrel is not compared. Missing components count as 0 (`6.10` = `6.10.0`).
fn btrfs_progs_older(recovery: Option<&str>, host: Option<&str>) -> Option<bool> {
    let (mut r, mut h) = (dotted_numbers(recovery?)?, dotted_numbers(host?)?);
    let len = r.len().max(h.len());
    r.resize(len, 0);
    h.resize(len, 0);
    Some(r < h)
}

/// What an age is counted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgeBasis {
    /// The last applied full upgrade.
    LastAppliedUpgrade,
    /// The install, when no full upgrade was ever applied.
    Install,
}

/// The verdict on one recovery OS.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Assessment {
    /// Whole days since the last applied full upgrade, else since the
    /// install; `None` when neither is known.
    pub age_days: Option<i64>,
    /// What `age_days` counts from; `None` exactly when it is `None`.
    pub age_basis: Option<AgeBasis>,
    pub stale: bool,
    /// Why it is stale; empty exactly when it is current.
    pub reasons: Vec<String>,
}

/// The problem recorded for `rel`, if it was unreadable.
fn problem_for<'a>(os: &'a RecoveryOs, rel: &str) -> Option<&'a str> {
    os.problems
        .iter()
        .map(String::as_str)
        .find(|p| p.strip_prefix(rel).is_some_and(|r| r.starts_with(": ")))
}

/// Why an item was not read: it could not be (`<unreadable>: could not read
/// <path>: <why>`), or it is not there (`<absent>: <path> is absent`).
fn unread(os: &RecoveryOs, rel: &str, unreadable: &str, absent: &str) -> String {
    match problem_for(os, rel) {
        Some(why) => format!("{unreadable}: could not read {why}"),
        None => format!("{absent}: {rel} is absent"),
    }
}

/// Whole days from `date` (what it is the date `of`) to `today`, or why
/// there is no age: the date is after today, or today does not parse.
fn age_since(of: &str, date: &str, today: &str) -> Result<i64, String> {
    match (day_number(today), day_number(date)) {
        (Some(t), Some(d)) if t < d => Err(format!(
            "{of} {date} is after today ({today}) — check the clock"
        )),
        (Some(t), Some(d)) => Ok(t - d),
        _ => Err("age unknown: today's date is unreadable".to_string()),
    }
}

/// Stale when the last APPLIED full upgrade — or, when none ever was, the
/// install — is older than `max_age_days`, unknown, or dated after today;
/// when a later attempt did not complete; when the newest kernel's series is
/// behind the host's, or either kernel is unknown; or when btrfs-progs is
/// older than the host's (when both versions can be compared). Unknown is
/// never current: the point is to nag.
pub fn assess(os: &RecoveryOs, host: &HostVersions, today: &str, max_age_days: u32) -> Assessment {
    let mut reasons = Vec::new();
    let mut age = None;
    let applied = os.last_full_upgrade_applied.as_deref();
    let attempted = os.last_full_upgrade_attempted.as_deref();
    let installed = os.installed.as_deref();
    let limit = i64::from(max_age_days);
    match applied {
        _ if !os.log_read => reasons.push(unread(
            os,
            PACMAN_LOG,
            "last upgrade unknown",
            "last upgrade unknown",
        )),
        Some(date) => match age_since("last full upgrade", date, today) {
            Ok(n) => {
                age = Some((n, AgeBasis::LastAppliedUpgrade));
                if n > limit {
                    reasons.push(format!(
                        "last full upgrade {n} days ago (limit {max_age_days})"
                    ));
                }
            }
            Err(why) => reasons.push(why),
        },
        None => {
            if let Some(date) = installed {
                match age_since("install", date, today) {
                    Ok(n) => {
                        age = Some((n, AgeBasis::Install));
                        if attempted.is_none() && n > limit {
                            reasons.push(format!(
                                "never upgraded since install on {date} ({})",
                                days(n)
                            ));
                        }
                    }
                    Err(why) => reasons.push(why),
                }
            }
            match (installed, attempted) {
                (Some(i), Some(a)) => reasons.push(format!(
                    "install {i}, never completed an upgrade; last attempt {a} did not complete"
                )),
                (None, Some(a)) => reasons.push(format!(
                    "no full system upgrade ever completed (last attempt {a})"
                )),
                (None, None) => {
                    reasons.push(format!("no full system upgrade recorded in {PACMAN_LOG}"))
                }
                (Some(_), None) => {}
            }
        }
    }
    if let (Some(done), Some(tried), false) = (applied, attempted, os.last_attempt_completed) {
        reasons.push(format!(
            "last upgrade attempt {tried} did not complete; last applied {done}"
        ));
    }
    let host_series = host.kernel.as_deref().and_then(kernel_series);
    match (newest_kernel(&os.kernels), host_series) {
        (None, _) if !os.modules_read => {
            reasons.push(unread(os, MODULES, "kernels unknown", "no kernel found"))
        }
        (None, _) if os.kernels.is_empty() => reasons.push("no kernel found".to_string()),
        (None, _) => reasons.push(format!(
            "kernel version unknown ({})",
            os.kernels.join(", ")
        )),
        (Some(_), None) => reasons.push("host kernel unknown, kernels not compared".to_string()),
        (Some(k), Some((hmaj, hmin))) => {
            if let Some((maj, min)) = kernel_series(k)
                && (maj, min) < (hmaj, hmin)
            {
                reasons.push(format!(
                    "kernel series {maj}.{min} ({k}) is behind the host's {hmaj}.{hmin}"
                ));
            }
        }
    }
    let progs = os.packages.get("btrfs-progs").map(String::as_str);
    if btrfs_progs_older(progs, host.btrfs_progs.as_deref()) == Some(true) {
        reasons.push(format!(
            "btrfs-progs {} is older than the host's {}",
            or_unknown(progs),
            or_unknown(host.btrfs_progs.as_deref())
        ));
    }
    Assessment {
        age_days: age.map(|(n, _)| n),
        age_basis: age.map(|(_, basis)| basis),
        stale: !reasons.is_empty(),
        reasons,
    }
}

/// What became of one recovery drive.
#[derive(Debug, Clone, PartialEq)]
pub enum DriveReport {
    /// The target is not mounted, so its OS was not looked at.
    NotMounted,
    /// The target is mounted but its OS root could not be read at all.
    Unreadable(String),
    Inspected {
        os: RecoveryOs,
        assessment: Assessment,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DriveEntry {
    pub label: String,
    /// The OS root that was (or would have been) read.
    pub root: PathBuf,
    pub report: DriveReport,
}

/// Inspect and assess the OS at `root`, or say why it cannot be read at all.
/// A root that is itself a symlink is refused: everything under its target
/// would pass the containment check, so a link to `/` would make the host
/// look like the recovery OS.
fn read_drive(
    root: &Path,
    host: &HostVersions,
    today: &str,
    max_age_days: u32,
) -> Result<(RecoveryOs, Assessment), String> {
    if root.is_symlink() {
        return Err(format!("{} is a symlink, not the OS root", root.display()));
    }
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let os = inspect(root);
    let assessment = assess(&os, host, today, max_age_days);
    Ok((os, assessment))
}

/// [`read_drive`] as a report entry; an unreadable root is
/// [`DriveReport::Unreadable`].
pub fn inspect_drive(
    label: &str,
    root: &Path,
    host: &HostVersions,
    today: &str,
    max_age_days: u32,
) -> DriveEntry {
    let report = match read_drive(root, host, today, max_age_days) {
        Ok((os, assessment)) => DriveReport::Inspected { os, assessment },
        Err(why) => DriveReport::Unreadable(why),
    };
    DriveEntry {
        label: label.to_string(),
        root: root.to_path_buf(),
        report,
    }
}

/// The OS root of a recovery drive mounted at `mount`.
pub fn os_root(mount: &str) -> PathBuf {
    Path::new(mount).join("@")
}

/// Every `role = "mirror"` target, in config order: inspected when it is a
/// mountpoint, [`DriveReport::NotMounted`] otherwise.
pub fn status_with(
    cfg: &Config,
    host: &HostVersions,
    today: &str,
    is_mounted: &dyn Fn(&Path) -> bool,
) -> Vec<DriveEntry> {
    let max = cfg.recovery_os.max_age_days;
    cfg.targets
        .iter()
        .filter(|t| t.role == TargetRole::Mirror)
        .map(|t| {
            let root = os_root(&t.mount);
            if is_mounted(Path::new(&t.mount)) {
                inspect_drive(&t.label, &root, host, today, max)
            } else {
                DriveEntry {
                    label: t.label.clone(),
                    root,
                    report: DriveReport::NotMounted,
                }
            }
        })
        .collect()
}

/// 2 if any mounted drive's root was unreadable, else 1 if any is stale, else 0.
pub fn exit_code(entries: &[DriveEntry]) -> i32 {
    entries
        .iter()
        .map(|e| match &e.report {
            DriveReport::Unreadable(_) => 2,
            DriveReport::Inspected { assessment, .. } if assessment.stale => 1,
            _ => 0,
        })
        .max()
        .unwrap_or(0)
}

fn or_unknown(v: Option<&str>) -> &str {
    v.unwrap_or("unknown")
}

fn days(n: i64) -> String {
    if n == 1 {
        "1 day".to_string()
    } else {
        format!("{n} days")
    }
}

fn package_text(os: &RecoveryOs, name: &str) -> String {
    match os.packages.get(name) {
        Some(v) => v.clone(),
        None if os.packages_read => "not installed".to_string(),
        None => "unknown".to_string(),
    }
}

/// `202 days since last applied upgrade`, `174 days since install`, or
/// `unknown`.
fn age_text(a: &Assessment) -> String {
    match (a.age_days, a.age_basis) {
        (Some(n), Some(AgeBasis::LastAppliedUpgrade)) => {
            format!("{} since last applied upgrade", days(n))
        }
        (Some(n), Some(AgeBasis::Install)) => format!("{} since install", days(n)),
        _ => "unknown".to_string(),
    }
}

/// The applied upgrade's date; with none, `none recorded` / `none completed`
/// when the log was read, else `unknown`.
fn upgrade_text(os: &RecoveryOs) -> String {
    match os.last_full_upgrade_applied.as_deref() {
        Some(d) => d.to_string(),
        None if !os.log_read => "unknown".to_string(),
        None if os.last_full_upgrade_attempted.is_some() => "none completed".to_string(),
        None => "none recorded".to_string(),
    }
}

/// The last attempt, when it is not the applied upgrade.
fn failed_attempt(os: &RecoveryOs) -> Option<&str> {
    (!os.last_attempt_completed)
        .then_some(os.last_full_upgrade_attempted.as_deref())
        .flatten()
}

/// `6.10-1 (host 7.1-1)`, with `; not compared` when both are known but are
/// not dotted numerics.
fn progs_text(os: &RecoveryOs, host: &HostVersions) -> String {
    let progs = os.packages.get("btrfs-progs").map(String::as_str);
    let host_progs = host.btrfs_progs.as_deref();
    let not_compared =
        progs.is_some() && host_progs.is_some() && btrfs_progs_older(progs, host_progs).is_none();
    format!(
        "{} (host {}{})",
        package_text(os, "btrfs-progs"),
        or_unknown(host_progs),
        if not_compared { "; not compared" } else { "" }
    )
}

fn verdict(a: &Assessment) -> String {
    if a.stale {
        format!("STALE — {}", a.reasons.join("; "))
    } else {
        "current".to_string()
    }
}

/// The `RECOVERY OS` block of the backup report.
pub fn format_section(entries: &[DriveEntry], host: &HostVersions) -> String {
    let mut out = String::from("RECOVERY OS\n");
    if entries.is_empty() {
        out.push_str("  no role = \"mirror\" targets configured\n");
    }
    let host_kernel = or_unknown(host.kernel.as_deref());
    for e in entries {
        let root = e.root.display();
        match &e.report {
            DriveReport::NotMounted => out.push_str(&format!("  {}  not mounted\n", e.label)),
            DriveReport::Unreadable(why) => {
                out.push_str(&format!("  {}  ({root})  UNREADABLE: {why}\n", e.label));
            }
            DriveReport::Inspected { os, assessment } => {
                let upgrade = upgrade_text(os);
                let kernel = or_unknown(newest_kernel(&os.kernels).map(String::as_str));
                let row = |k: &str, v: &str| format!("    {k:<20}{v}\n");
                out.push_str(&format!("  {}  ({root})\n", e.label));
                out.push_str(&row("OS", or_unknown(os.os_name.as_deref())));
                out.push_str(&row("Installed", or_unknown(os.installed.as_deref())));
                out.push_str(&row("Last full upgrade", &upgrade));
                if let Some(tried) = failed_attempt(os) {
                    out.push_str(&row("Last attempt", &format!("{tried} (did not complete)")));
                }
                out.push_str(&row("Age", &age_text(assessment)));
                out.push_str(&row("Kernel", &format!("{kernel} (host {host_kernel})")));
                out.push_str(&row("btrfs-progs", &progs_text(os, host)));
                out.push_str(&row("btrbk", &package_text(os, "btrbk")));
                out.push_str(&row(
                    "das-backup-manager",
                    &package_text(os, "das-backup-manager"),
                ));
                for p in &os.problems {
                    out.push_str(&row("Could not read", p));
                }
                out.push_str(&row("Result", &verdict(assessment)));
            }
        }
    }
    out
}

/// The record `--state-file` keeps, one entry per drive label.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredState {
    pub schema_version: u32,
    pub drives: BTreeMap<String, StoredDrive>,
}

/// The last reading of one drive: the facts, not the verdict, so `health`
/// can re-assess them against today and the host as it is now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredDrive {
    pub checked_epoch: i64,
    pub os: Option<RecoveryOs>,
    pub error: Option<String>,
}

/// 2 since `installed` was added; a record of any other version is refused.
const STATE_SCHEMA_VERSION: u32 = 2;

/// The state file, overridable for tests and the VM rig via
/// `DAS_RECOVERY_OS_STATE`.
pub fn state_path() -> PathBuf {
    PathBuf::from(
        std::env::var("DAS_RECOVERY_OS_STATE")
            .unwrap_or_else(|_| RECOVERY_OS_STATE_PATH.to_string()),
    )
}

/// `Ok(None)` when there is no file yet; an unreadable or corrupt file, or a
/// record of another schema version, is an error — never an empty record —
/// that names the file and the command that clears it.
pub fn load_state(path: &Path) -> Result<Option<StoredState>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let fail = |why: String| {
        let p = path.display();
        format!("{p}: {why} — left as it is; remove it to start over: rm -- '{p}'")
    };
    let text = fs::read_to_string(path).map_err(|e| fail(e.to_string()))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| fail(e.to_string()))?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64);
    if version != Some(u64::from(STATE_SCHEMA_VERSION)) {
        return Err(fail(format!(
            "record schema version {}, this btrdasd reads {STATE_SCHEMA_VERSION}",
            version.map_or_else(|| "none".to_string(), |v| v.to_string())
        )));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| fail(e.to_string()))
}

/// Record this run's readings. A drive that was not mounted keeps its earlier
/// record. An existing file that cannot be read is an error and is left as it
/// is — overwriting it would lose the other drive's last reading. Written
/// atomically, mode 0644 (`health` runs unprivileged).
pub fn write_state(path: &Path, entries: &[DriveEntry], now_epoch: i64) -> Result<(), String> {
    let mut state = load_state(path)?.unwrap_or_default();
    state.schema_version = STATE_SCHEMA_VERSION;
    for e in entries {
        let record = match &e.report {
            DriveReport::NotMounted => continue,
            DriveReport::Unreadable(why) => StoredDrive {
                checked_epoch: now_epoch,
                os: None,
                error: Some(why.clone()),
            },
            DriveReport::Inspected { os, .. } => StoredDrive {
                checked_epoch: now_epoch,
                os: Some(os.clone()),
                error: None,
            },
        };
        state.drives.insert(e.label.clone(), record);
    }
    let text = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
    // 0644 whatever the file had (`health` runs unprivileged), set before the
    // new file is renamed into place. The error names the file.
    crate::fsutil::write_atomic_mode(path, format!("{text}\n").as_bytes(), Some(0o644))
        .map_err(|e| e.to_string())
}

/// `YYYY-MM-DD HH:MM UTC`.
pub fn format_epoch_utc(epoch: i64) -> String {
    let secs = epoch.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02} UTC",
        date_of(epoch.div_euclid(86_400)),
        secs / 3600,
        secs % 3600 / 60
    )
}

/// What `btrdasd health` shows about the recovery OSes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecoveryHealth {
    /// One summary line per mirror target.
    pub lines: Vec<String>,
    /// Stale, unreadable or never-checked drives; these make health WARNING.
    pub warnings: Vec<String>,
}

/// `<label> (<when>): installed …, last full upgrade …, age …, kernel … —
/// current|STALE: …`.
fn summary(label: &str, when: &str, os: &RecoveryOs, a: &Assessment) -> String {
    let upgrade = upgrade_text(os);
    let kernel = or_unknown(newest_kernel(&os.kernels).map(String::as_str));
    let state = if a.stale {
        format!("STALE: {}", a.reasons.join("; "))
    } else {
        "current".to_string()
    };
    let unreadable = match os.problems.len() {
        0 => String::new(),
        1 => "; 1 path unreadable".to_string(),
        n => format!("; {n} paths unreadable"),
    };
    format!(
        "{label} ({when}): installed {}, last full upgrade {upgrade}, age {}, kernel {kernel} \
         — {state}{unreadable}",
        or_unknown(os.installed.as_deref()),
        age_text(a)
    )
}

/// Live readings for mounted mirror targets, the stored record for the rest.
/// The stored facts are re-assessed against `today` and the host now, so an
/// age keeps growing between runs. `probe_host` runs at most once, and only
/// when there is a reading to compare (it spawns `uname` and `pacman`).
pub fn health_with(
    cfg: &Config,
    state: &Result<Option<StoredState>, String>,
    probe_host: &dyn Fn() -> HostVersions,
    today: &str,
    is_mounted: &dyn Fn(&Path) -> bool,
) -> RecoveryHealth {
    let mut h = RecoveryHealth::default();
    let max = cfg.recovery_os.max_age_days;
    let mut state_reported = false;
    let host_cell = std::cell::OnceCell::new();
    let host = || host_cell.get_or_init(probe_host);
    for t in cfg.targets.iter().filter(|t| t.role == TargetRole::Mirror) {
        let label = &t.label;
        if is_mounted(Path::new(&t.mount)) {
            match read_drive(&os_root(&t.mount), host(), today, max) {
                Ok((os, assessment)) => {
                    h.lines.push(summary(label, "live", &os, &assessment));
                    if assessment.stale {
                        h.warnings.push(format!(
                            "Recovery OS on '{label}' is STALE: {}",
                            assessment.reasons.join("; ")
                        ));
                    }
                }
                Err(why) => {
                    h.lines
                        .push(format!("{label} (live): OS root unreadable: {why}"));
                    h.warnings
                        .push(format!("Recovery OS on '{label}' could not be read: {why}"));
                }
            }
            continue;
        }
        let stored = match state {
            Err(err) => {
                h.lines.push(format!(
                    "{label}: not mounted; stored record unreadable: {err}"
                ));
                if !state_reported {
                    h.warnings
                        .push(format!("Recovery OS state unreadable: {err}"));
                    state_reported = true;
                }
                continue;
            }
            Ok(s) => s.as_ref().and_then(|s| s.drives.get(label)),
        };
        match stored {
            None => {
                h.lines
                    .push(format!("{label}: not mounted and never checked"));
                h.warnings
                    .push(format!("Recovery OS on '{label}' has never been checked"));
            }
            Some(StoredDrive {
                checked_epoch,
                os: Some(os),
                ..
            }) => {
                let a = assess(os, host(), today, max);
                let when = format_epoch_utc(*checked_epoch);
                h.lines
                    .push(summary(label, &format!("as of {when}"), os, &a));
                if a.stale {
                    h.warnings.push(format!(
                        "Recovery OS on '{label}' is STALE: {}",
                        a.reasons.join("; ")
                    ));
                }
            }
            Some(StoredDrive {
                checked_epoch,
                error,
                ..
            }) => {
                let why = or_unknown(error.as_deref());
                let when = format_epoch_utc(*checked_epoch);
                h.lines
                    .push(format!("{label} (as of {when}): OS root unreadable: {why}"));
                h.warnings
                    .push(format!("Recovery OS on '{label}' could not be read: {why}"));
            }
        }
    }
    h
}

/// The outcome of `btrdasd recovery-os status`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusRun {
    pub entries: Vec<DriveEntry>,
    /// Why the state file could not be written, when it could not.
    pub state_error: Option<String>,
    /// 0 current, 1 stale, 2 a root or the state file could not be handled.
    pub code: i32,
}

/// [`status_with`], then the state file when one is named.
pub fn status_run_with(
    cfg: &Config,
    state_file: Option<&Path>,
    host: &HostVersions,
    today: &str,
    now_epoch: i64,
    is_mounted: &dyn Fn(&Path) -> bool,
) -> StatusRun {
    let entries = status_with(cfg, host, today, is_mounted);
    let state_error = state_file.and_then(|p| write_state(p, &entries, now_epoch).err());
    let code = if state_error.is_some() {
        2
    } else {
        exit_code(&entries)
    };
    StatusRun {
        entries,
        state_error,
        code,
    }
}

/// One drive as JSON: `status` is `current`, `stale`, `unreadable` or
/// `not_mounted`; `os` and `assessment` are null unless it was inspected.
pub fn entry_json(entry: &DriveEntry) -> serde_json::Value {
    let (status, os, assessment, error) = match &entry.report {
        DriveReport::NotMounted => ("not_mounted", None, None, None),
        DriveReport::Unreadable(why) => ("unreadable", None, None, Some(why.as_str())),
        DriveReport::Inspected { os, assessment } => (
            if assessment.stale { "stale" } else { "current" },
            Some(os),
            Some(assessment),
            None,
        ),
    };
    serde_json::json!({
        "label": entry.label,
        "root": entry.root.display().to_string(),
        "status": status,
        "error": error,
        "os": os,
        "assessment": assessment,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caldate::{date_of, day_number};
    use crate::fsutil::testing::Scripted;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const TODAY: &str = "2026-10-02";

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn desc(name: &str, version: &str) -> String {
        format!("%NAME%\n{name}\n\n%VERSION%\n{version}\n\n%BASE%\n{name}\n\n%DESC%\nx\n")
    }

    fn add_pkg(root: &Path, dir: &str, name: &str, version: &str) {
        write(
            root,
            &format!("var/lib/pacman/local/{dir}/desc"),
            &desc(name, version),
        );
    }

    /// A recovery OS root as a CachyOS install lays it out.
    fn full_root(root: &Path) {
        write(
            root,
            "usr/lib/os-release",
            "NAME=\"CachyOS Linux\"\nPRETTY_NAME=\"CachyOS\"\nID=cachyos\n",
        );
        fs::create_dir_all(root.join("etc")).unwrap();
        std::os::unix::fs::symlink("../usr/lib/os-release", root.join("etc/os-release")).unwrap();
        fs::create_dir_all(root.join("usr/lib/modules/6.12.1-1-cachyos")).unwrap();
        fs::create_dir_all(root.join("usr/lib/modules/6.6.5-2-cachyos-lts")).unwrap();
        write(
            root,
            "var/log/pacman.log",
            "[2026-01-02T10:00:00+0100] [PACMAN] Running 'pacman -Syu'\n\
             [2026-01-02T10:00:01+0100] [PACMAN] starting full system upgrade\n\
             [2026-01-02T10:01:00+0100] [ALPM] transaction started\n\
             [2026-01-02T10:01:00+0100] [ALPM] upgraded foo (1-1 -> 2-1)\n\
             [2026-01-02T10:01:30+0100] [ALPM] transaction completed\n\
             [2026-03-14T09:30:12+0100] [PACMAN] Running 'pacman -Syu'\n\
             [2026-03-14T09:30:15+0100] [PACMAN] starting full system upgrade\n\
             [2026-03-14T09:31:00+0100] [ALPM] transaction started\n\
             [2026-03-14T09:32:00+0100] [ALPM] transaction completed\n\
             [2026-03-20T11:00:00+0100] [PACMAN] Running 'pacman -S vim'\n\
             [2026-03-20T11:00:05+0100] [ALPM] transaction completed\n",
        );
        add_pkg(root, "btrfs-progs-6.10-1", "btrfs-progs", "6.10-1");
        add_pkg(root, "btrbk-0.32.6-1", "btrbk", "0.32.6-1");
        add_pkg(root, "linux-cachyos-6.12.1-1", "linux-cachyos", "6.12.1-1");
        // A prefix of a watched name must not be taken for it.
        add_pkg(
            root,
            "linux-cachyos-lts-6.6.5-2",
            "linux-cachyos-lts",
            "6.6.5-2",
        );
        add_pkg(root, "vim-9.1-1", "vim", "9.1-1");
        // The ALPM_DB_VERSION file pacman keeps next to the entries.
        write(root, "var/lib/pacman/local/ALPM_DB_VERSION", "9\n");
    }

    fn host(kernel: &str) -> HostVersions {
        HostVersions {
            kernel: Some(kernel.into()),
            btrfs_progs: Some("6.17-1".into()),
        }
    }

    fn os_with(upgrade: Option<&str>, kernels: &[&str]) -> RecoveryOs {
        RecoveryOs {
            os_name: Some("CachyOS".into()),
            last_full_upgrade_applied: upgrade.map(String::from),
            last_full_upgrade_attempted: upgrade.map(String::from),
            last_attempt_completed: upgrade.is_some(),
            log_read: true,
            kernels: kernels.iter().map(|k| k.to_string()).collect(),
            modules_read: true,
            packages_read: true,
            ..Default::default()
        }
    }

    fn days_before(today: &str, n: i64) -> String {
        date_of(day_number(today).unwrap() - n)
    }

    // ---- inspect --------------------------------------------------------

    #[test]
    fn inspect_reads_every_item_of_a_complete_root() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        let os = inspect(dir.path());
        assert_eq!(os.os_name.as_deref(), Some("CachyOS"));
        assert_eq!(os.last_full_upgrade_applied.as_deref(), Some("2026-03-14"));
        assert_eq!(
            os.last_full_upgrade_attempted.as_deref(),
            Some("2026-03-14")
        );
        assert!(os.last_attempt_completed);
        assert_eq!(
            os.installed.as_deref(),
            Some("2026-01-02"),
            "the first line"
        );
        assert!(os.log_read && os.modules_read);
        assert_eq!(os.kernels, ["6.12.1-1-cachyos", "6.6.5-2-cachyos-lts"]);
        assert!(os.packages_read);
        let want: BTreeMap<String, String> = [
            ("btrbk", "0.32.6-1"),
            ("btrfs-progs", "6.10-1"),
            ("linux-cachyos", "6.12.1-1"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        assert_eq!(os.packages, want, "only watched names, matched exactly");
        assert!(os.problems.is_empty(), "{:?}", os.problems);
    }

    #[test]
    fn inspect_never_updates_an_access_time() {
        // A read that bumps atime is a write to the recovery OS (it moved the
        // subvolume's generation on a real drive). Old atimes below mtime make
        // relatime update on the first read, so this fails without O_NOATIME.
        let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        // Positive control: only a filesystem where a plain read DOES move the
        // atime can show this. /tmp is often noatime; /dev/shm usually is not.
        let dir = ["/dev/shm", &std::env::temp_dir().to_string_lossy()]
            .iter()
            .filter_map(|base| tempfile::tempdir_in(base).ok())
            .find(|d| {
                let probe = d.path().join("probe");
                fs::write(&probe, "x").unwrap();
                filetime::set_file_atime(&probe, old).unwrap();
                fs::read(&probe).unwrap();
                filetime::FileTime::from_last_access_time(&fs::metadata(&probe).unwrap()) != old
            });
        let Some(dir) = dir else {
            eprintln!("SKIPPED: no writable filesystem here updates atime on read");
            return;
        };
        full_root(dir.path());
        let paths: Vec<PathBuf> = [
            "usr/lib/os-release",
            "var/log/pacman.log",
            "usr/lib/modules",
            "var/lib/pacman/local",
            "var/lib/pacman/local/btrbk-0.32.6-1/desc",
        ]
        .iter()
        .map(|r| dir.path().join(r))
        .collect();
        for p in &paths {
            filetime::set_file_atime(p, old).unwrap();
        }
        // Following a symlink updates the link's own atime; no open flag stops
        // that, so the etc/os-release link must not be followed at all.
        let link = dir.path().join("etc/os-release");
        filetime::set_symlink_file_times(&link, old, old).unwrap();
        let os = inspect(dir.path());
        assert!(os.problems.is_empty(), "{:?}", os.problems);
        assert_eq!(
            os.last_full_upgrade_applied.as_deref(),
            Some("2026-03-14"),
            "it did read"
        );
        assert_eq!(
            os.os_name.as_deref(),
            Some("CachyOS"),
            "via usr/lib/os-release"
        );
        for p in &paths {
            let atime = filetime::FileTime::from_last_access_time(&fs::metadata(p).unwrap());
            assert_eq!(atime, old, "atime of {} changed", p.display());
        }
        let atime =
            filetime::FileTime::from_last_access_time(&fs::symlink_metadata(&link).unwrap());
        assert_eq!(atime, old, "atime of the etc/os-release symlink changed");
    }

    #[test]
    fn a_file_that_needs_an_atime_update_to_read_is_refused() {
        // Unprivileged, a file someone else owns cannot be opened O_NOATIME.
        // /usr/lib/os-release is a regular, root-owned file on every
        // distribution this runs on (/etc/os-release is usually a link to it).
        // As root, or as its owner, there is nothing to refuse.
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = fs::symlink_metadata("/usr/lib/os-release") else {
            eprintln!("SKIPPED: no /usr/lib/os-release on this system");
            return;
        };
        // SAFETY: geteuid() is always safe.
        let euid = unsafe { libc::geteuid() };
        if !meta.is_file() || euid == 0 || meta.uid() == euid {
            eprintln!("SKIPPED: not a regular file, or running as root or its owner");
            return;
        }
        assert_eq!(
            read_in_root(Path::new("/"), "usr/lib/os-release"),
            Err(ReadErr::Unreadable(
                "usr/lib/os-release: cannot be read without updating its access time (run as root)"
                    .into()
            ))
        );
        assert_eq!(
            open_error("x", &std::io::Error::from_raw_os_error(libc::ENOENT)),
            format!("x: {}", std::io::Error::from_raw_os_error(libc::ENOENT))
        );
    }

    #[test]
    fn inspect_of_an_empty_root_is_absent_everywhere_and_unreadable_nowhere() {
        let dir = tempfile::tempdir().unwrap();
        let os = inspect(dir.path());
        assert_eq!(os.os_name, None);
        assert_eq!(os.last_full_upgrade_applied, None);
        assert_eq!(os.last_full_upgrade_attempted, None);
        assert_eq!(os.installed, None);
        assert!(os.kernels.is_empty());
        assert!(os.packages.is_empty());
        assert!(!os.log_read && !os.modules_read);
        assert!(
            !os.packages_read,
            "an unread database is not 'nothing installed'"
        );
        assert!(
            os.problems.is_empty(),
            "absent is not unreadable: {:?}",
            os.problems
        );
        let a = assess(&os, &host("7.2.8"), TODAY, 60);
        assert_eq!(
            a.reasons,
            [
                "last upgrade unknown: var/log/pacman.log is absent",
                "no kernel found: usr/lib/modules is absent"
            ]
        );
    }

    #[test]
    fn an_unreadable_item_is_reported_as_unreadable_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        // A directory where the log should be, a file where the modules are.
        fs::remove_file(dir.path().join("var/log/pacman.log")).unwrap();
        fs::create_dir(dir.path().join("var/log/pacman.log")).unwrap();
        fs::remove_dir_all(dir.path().join("usr/lib/modules")).unwrap();
        write(dir.path(), "usr/lib/modules", "not a directory");
        let os = inspect(dir.path());
        assert!(!os.log_read && !os.modules_read);
        assert!(
            os.problems
                .contains(&"var/log/pacman.log: not a regular file".to_string()),
            "{:?}",
            os.problems
        );
        let modules_problem = os
            .problems
            .iter()
            .find(|p| p.starts_with("usr/lib/modules: "))
            .expect("modules problem")
            .clone();
        let a = assess(&os, &host("7.2.8"), TODAY, 60);
        assert_eq!(
            a.reasons[..2],
            [
                "last upgrade unknown: could not read var/log/pacman.log: not a regular file"
                    .to_string(),
                format!("kernels unknown: could not read {modules_problem}"),
            ]
        );
    }

    #[test]
    fn a_log_without_an_upgrade_line_is_no_date_and_not_a_read_problem() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        write(
            dir.path(),
            "var/log/pacman.log",
            "[2026-03-14T09:30:12+0100] [PACMAN] Running 'pacman -S vim'\n",
        );
        let os = inspect(dir.path());
        assert_eq!(os.last_full_upgrade_applied, None);
        assert_eq!(os.last_full_upgrade_attempted, None);
        assert!(os.log_read);
        assert!(os.problems.is_empty(), "{:?}", os.problems);
        assert_eq!(os.installed.as_deref(), Some("2026-03-14"));
        let a = assess(&os, &host("7.2.8"), TODAY, 60);
        assert_eq!(
            a.reasons[0],
            "never upgraded since install on 2026-03-14 (202 days)"
        );
    }

    fn upgrades(log: &str) -> RecoveryOs {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        write(dir.path(), "var/log/pacman.log", log);
        inspect(dir.path())
    }

    #[test]
    fn only_an_upgrade_whose_transaction_completed_counts_as_applied() {
        // Applied, then a later attempt that never completed (a keyring
        // failure, or answered "n").
        let os = upgrades(
            "[2026-03-14T09:30:12+0100] [PACMAN] Running 'pacman -Syu'\n\
             [2026-03-14T09:30:15+0100] [PACMAN] starting full system upgrade\n\
             [2026-03-14T09:31:00+0100] [ALPM] transaction started\n\
             [2026-03-14T09:32:00+0100] [ALPM] transaction completed\n\
             [2026-10-01T08:00:00+0200] [PACMAN] Running 'pacman -Syu'\n\
             [2026-10-01T08:00:02+0200] [PACMAN] starting full system upgrade\n\
             [2026-10-01T08:00:09+0200] [PACMAN] error: archlinux-keyring: signature is unknown trust\n",
        );
        assert_eq!(os.last_full_upgrade_applied.as_deref(), Some("2026-03-14"));
        assert_eq!(
            os.last_full_upgrade_attempted.as_deref(),
            Some("2026-10-01")
        );
        assert!(!os.last_attempt_completed);

        // Started but never completed: not applied.
        let os = upgrades(
            "[2026-03-14T09:30:15+0100] [PACMAN] starting full system upgrade\n\
             [2026-03-14T09:31:00+0100] [ALPM] transaction started\n\
             [2026-03-14T09:31:05+0100] [ALPM] upgraded foo (1-1 -> 2-1)\n",
        );
        assert_eq!(os.last_full_upgrade_applied, None);
        assert_eq!(
            os.last_full_upgrade_attempted.as_deref(),
            Some("2026-03-14")
        );

        // A completion that belongs to a later invocation is not this one's.
        let os = upgrades(
            "[2026-03-14T09:30:15+0100] [PACMAN] starting full system upgrade\n\
             [2026-03-14T09:40:00+0100] [PACMAN] Running 'pacman -S vim'\n\
             [2026-03-14T09:40:05+0100] [ALPM] transaction completed\n",
        );
        assert_eq!(os.last_full_upgrade_applied, None);

        // An undatable marker closes the window: a completion after it is not
        // credited to the earlier marker.
        let os = upgrades(
            "[2026-03-14T09:30:15+0100] [PACMAN] starting full system upgrade\n\
             [not-a-date] [PACMAN] starting full system upgrade\n\
             [2026-03-14T09:40:05+0100] [ALPM] transaction completed\n",
        );
        assert_eq!(os.last_full_upgrade_applied, None);
        assert_eq!(
            os.last_full_upgrade_attempted.as_deref(),
            Some("2026-03-14")
        );
    }

    #[test]
    fn a_failed_last_attempt_is_stale_and_says_both_dates() {
        let mut os = os_with(Some(&days_before(TODAY, 3)), &["7.2.1"]);
        os.last_full_upgrade_attempted = Some(TODAY.to_string());
        os.last_attempt_completed = false;
        let a = assess(&os, &host("7.2.8"), TODAY, 60);
        assert!(a.stale);
        assert_eq!(a.age_days, Some(3), "the age is the applied upgrade's");
        assert_eq!(
            a.reasons,
            ["last upgrade attempt 2026-10-02 did not complete; last applied 2026-09-29"]
        );
        let mut never = os_with(None, &["7.2.1"]);
        never.last_full_upgrade_attempted = Some("2026-10-01".into());
        let a = assess(&never, &host("7.2.8"), TODAY, 60);
        assert_eq!(a.age_days, None);
        assert_eq!(
            a.reasons,
            ["no full system upgrade ever completed (last attempt 2026-10-01)"]
        );
    }

    /// The production recovery OSes' log: installed from the live ISO with
    /// `pacman -b`, then never a full upgrade.
    const NEVER_UPGRADED_LOG: &str = "\
[2026-04-12T23:23:05-0500] [PACMAN] Running 'pacman -b /mnt/var/lib/pacman -r /mnt -S base'
[2026-04-12T23:23:06-0500] [ALPM] transaction started
[2026-04-12T23:30:00-0500] [ALPM] installed base (3-2)
[2026-04-12T23:31:00-0500] [ALPM] transaction completed
[2026-05-01T10:00:00-0500] [PACMAN] Running 'pacman -S vim'
[2026-05-01T10:00:05-0500] [ALPM] transaction completed
";

    /// A host the full_root fixture is otherwise current against.
    fn same_host() -> HostVersions {
        HostVersions {
            kernel: Some("6.12.9-1-cachyos".into()),
            btrfs_progs: Some("6.10-1".into()),
        }
    }

    fn install_plus(n: i64) -> String {
        date_of(day_number("2026-04-12").unwrap() + n)
    }

    #[test]
    fn a_never_upgraded_install_is_aged_from_its_install_date() {
        let os = upgrades(NEVER_UPGRADED_LOG);
        assert_eq!(os.installed.as_deref(), Some("2026-04-12"));
        assert_eq!(os.last_full_upgrade_applied, None);
        assert_eq!(os.last_full_upgrade_attempted, None);
        let a = assess(&os, &same_host(), "2026-10-03", 60);
        assert!(a.stale);
        assert_eq!(a.age_days, Some(174));
        assert_eq!(a.age_basis, Some(AgeBasis::Install));
        assert_eq!(
            a.reasons,
            ["never upgraded since install on 2026-04-12 (174 days)"]
        );
        // Ten days after the install it is current: the same threshold.
        let a = assess(&os, &same_host(), &install_plus(10), 60);
        assert_eq!((a.age_days, a.stale), (Some(10), false), "{:?}", a.reasons);
        assert_eq!(a.age_basis, Some(AgeBasis::Install));
        assert!(!assess(&os, &same_host(), &install_plus(60), 60).stale);
        assert_eq!(
            assess(&os, &same_host(), &install_plus(61), 60).reasons,
            ["never upgraded since install on 2026-04-12 (61 days)"]
        );
        // A kernel series behind the host's is stale whatever the age.
        let a = assess(&os, &host("7.2.8"), &install_plus(10), 60);
        assert!(a.stale);
        assert_eq!(
            a.reasons,
            [
                "kernel series 6.12 (6.12.1-1-cachyos) is behind the host's 7.2",
                "btrfs-progs 6.10-1 is older than the host's 6.17-1",
            ]
        );
    }

    #[test]
    fn a_log_whose_first_line_has_no_date_has_no_install_date() {
        let os = upgrades(&format!(
            "garbage before the first entry\n{NEVER_UPGRADED_LOG}"
        ));
        assert_eq!(os.installed, None, "never a later line's date");
        assert!(os.log_read);
        let a = assess(&os, &same_host(), "2026-10-03", 60);
        assert!(a.stale);
        assert_eq!((a.age_days, a.age_basis), (None, None), "no fabricated age");
        assert_eq!(
            a.reasons,
            ["no full system upgrade recorded in var/log/pacman.log"]
        );
        assert_eq!(upgrades("").installed, None, "an empty log");
    }

    #[test]
    fn a_never_upgraded_install_with_a_failed_attempt_says_both() {
        let os = upgrades(&format!(
            "{NEVER_UPGRADED_LOG}\
             [2026-09-30T08:00:00-0500] [PACMAN] Running 'pacman -Syu'\n\
             [2026-09-30T08:00:02-0500] [PACMAN] starting full system upgrade\n\
             [2026-09-30T08:00:09-0500] [PACMAN] error: archlinux-keyring: signature is unknown trust\n"
        ));
        assert_eq!(os.installed.as_deref(), Some("2026-04-12"));
        let a = assess(&os, &same_host(), "2026-10-03", 60);
        assert!(a.stale);
        assert_eq!(
            (a.age_days, a.age_basis),
            (Some(174), Some(AgeBasis::Install))
        );
        assert_eq!(
            a.reasons,
            [
                "install 2026-04-12, never completed an upgrade; last attempt 2026-09-30 did not complete"
            ]
        );
        // Stale even when the install is recent: the attempt failed.
        let a = assess(&os, &same_host(), &install_plus(10), 60);
        assert!(a.stale);
        assert_eq!(a.reasons.len(), 1, "{:?}", a.reasons);
    }

    #[test]
    fn an_install_date_after_today_or_an_unreadable_today_has_no_age() {
        let mut os = os_with(None, &["7.2.1"]);
        os.installed = Some("2026-10-05".into());
        let a = assess(&os, &host("7.2.8"), TODAY, 60);
        assert!(a.stale);
        assert_eq!((a.age_days, a.age_basis), (None, None));
        assert_eq!(
            a.reasons,
            ["install 2026-10-05 is after today (2026-10-02) — check the clock"]
        );
        os.installed = Some("2026-10-01".into());
        let a = assess(&os, &host("7.2.8"), "garbage", 60);
        assert!(a.stale);
        assert_eq!((a.age_days, a.age_basis), (None, None));
        assert_eq!(a.reasons, ["age unknown: today's date is unreadable"]);
        // An applied upgrade is the basis, however old the install.
        let mut os = os_with(Some(&days_before(TODAY, 5)), &["7.2.1"]);
        os.installed = Some("2020-01-01".into());
        let a = assess(&os, &host("7.2.8"), TODAY, 60);
        assert_eq!(
            (a.age_days, a.age_basis, a.stale),
            (Some(5), Some(AgeBasis::LastAppliedUpgrade), false)
        );
    }

    #[test]
    fn btrfs_progs_older_than_the_host_is_stale_when_both_versions_are_numeric() {
        let up = days_before(TODAY, 1);
        let with = |v: &str, h: &str| {
            let mut os = os_with(Some(&up), &["7.2.1"]);
            os.packages.insert("btrfs-progs".into(), v.into());
            let host = HostVersions {
                kernel: Some("7.2.8".into()),
                btrfs_progs: Some(h.into()),
            };
            assess(&os, &host, TODAY, 60)
        };
        assert_eq!(
            with("6.10-1", "7.1-1").reasons,
            ["btrfs-progs 6.10-1 is older than the host's 7.1-1"]
        );
        assert_eq!(with("6.10-1", "6.10.1-1").reasons.len(), 1, "6.10 < 6.10.1");
        assert!(
            with("7.1-1", "7.1-2").reasons.is_empty(),
            "pkgrel is not compared"
        );
        assert!(with("7.2-1", "7.1-1").reasons.is_empty(), "newer is fine");
        assert!(with("6.10.1-1", "6.10-1").reasons.is_empty());
        assert!(
            with("6.10.r3.gabc-1", "7.1-1").reasons.is_empty(),
            "no verdict"
        );
        assert!(
            with("1:6.0-1", "7.1-1").reasons.is_empty(),
            "an epoch: no verdict"
        );
        assert!(with("6.10-1", "garbage").reasons.is_empty());
        // Not installed or unknown: no verdict from this check.
        let os = os_with(Some(&up), &["7.2.1"]);
        assert!(assess(&os, &host("7.2.8"), TODAY, 60).reasons.is_empty());
        assert_eq!(btrfs_progs_older(Some("6.10-1"), Some("7.1-1")), Some(true));
        assert_eq!(btrfs_progs_older(Some("7.1-1"), Some("7.1-1")), Some(false));
        assert_eq!(btrfs_progs_older(Some("x-1"), Some("7.1-1")), None);
        assert_eq!(btrfs_progs_older(None, Some("7.1-1")), None);
        assert_eq!(btrfs_progs_older(Some("7.1-1"), None), None);
    }

    #[test]
    fn the_old_log_date_format_and_stray_bytes_are_read() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        let mut bytes = b"[2019-05-01 10:22] [PACMAN] starting full system upgrade\n\
                              [2019-05-01 10:25] [ALPM] transaction completed\n"
            .to_vec();
        bytes.extend_from_slice(
            b"\xff\xfe garbage\n[not-a-date] [PACMAN] starting full system upgrade\n",
        );
        fs::write(dir.path().join("var/log/pacman.log"), bytes).unwrap();
        let os = inspect(dir.path());
        assert_eq!(
            os.last_full_upgrade_applied.as_deref(),
            Some("2019-05-01"),
            "an unparsable date is skipped, not taken"
        );
        assert_eq!(
            os.last_full_upgrade_attempted.as_deref(),
            Some("2019-05-01")
        );
    }

    #[test]
    fn os_release_falls_back_to_name_and_strips_quotes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "etc/os-release", "ID=arch\nNAME='Arch Linux'\n");
        assert_eq!(inspect(dir.path()).os_name.as_deref(), Some("Arch Linux"));
        write(dir.path(), "etc/os-release", "ID=arch\nPRETTY_NAME=\"\"\n");
        let os = inspect(dir.path());
        assert_eq!(os.os_name, None, "an empty name is no name");
    }

    #[test]
    fn a_symlink_that_leaves_the_os_root_is_never_followed() {
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "os-release", "PRETTY_NAME=\"The Host\"\n");
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("etc")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("os-release"),
            dir.path().join("etc/os-release"),
        )
        .unwrap();
        let os = inspect(dir.path());
        assert_eq!(os.os_name, None, "the host's name must never be reported");
        assert!(
            os.problems
                .iter()
                .any(|p| p.starts_with("etc/os-release: ")
                    && p.ends_with("is a symlink, not followed")),
            "{:?}",
            os.problems
        );
    }

    #[test]
    fn no_symlink_inside_the_root_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        // var/log as a link to a real directory with a log in it.
        let elsewhere = dir.path().join("elsewhere");
        fs::rename(dir.path().join("var/log"), &elsewhere).unwrap();
        std::os::unix::fs::symlink("../elsewhere", dir.path().join("var/log")).unwrap();
        // A kernel entry that is a link is not a kernel.
        std::os::unix::fs::symlink(
            "6.12.1-1-cachyos",
            dir.path().join("usr/lib/modules/9.9.9-linked"),
        )
        .unwrap();
        let os = inspect(dir.path());
        assert_eq!(os.last_full_upgrade_applied, None);
        let var_log = dir.path().join("var/log");
        assert!(
            os.problems.contains(&format!(
                "var/log/pacman.log: {} is a symlink, not followed",
                var_log.display()
            )),
            "{:?}",
            os.problems
        );
        assert_eq!(os.kernels, ["6.12.1-1-cachyos", "6.6.5-2-cachyos-lts"]);
        assert_eq!(
            resolve_in_root(dir.path(), "../etc/passwd"),
            Err(ReadErr::Unreadable(
                "../etc/passwd: not a plain relative path".into()
            ))
        );
        assert_eq!(
            resolve_in_root(dir.path(), "/etc/passwd"),
            Err(ReadErr::Unreadable(
                "/etc/passwd: not a plain relative path".into()
            ))
        );
        assert_eq!(
            resolve_in_root(dir.path(), "usr/lib"),
            Ok(dir.path().join("usr/lib"))
        );
    }

    #[test]
    fn the_open_flags_refuse_a_symlink_and_a_file_posing_as_a_directory() {
        // Defence in depth behind resolve_in_root's lstat walk: a link swapped
        // in afterwards is still not followed, and a directory read never
        // opens a file.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f", "x");
        std::os::unix::fs::symlink("f", dir.path().join("link")).unwrap();
        fs::create_dir(dir.path().join("d")).unwrap();
        std::os::unix::fs::symlink("d", dir.path().join("dlink")).unwrap();
        let errno = |r: std::io::Result<fs::File>| r.err().and_then(|e| e.raw_os_error());
        assert!(open_noatime(&dir.path().join("f"), FILE_FLAGS).is_ok());
        assert_eq!(
            errno(open_noatime(&dir.path().join("link"), FILE_FLAGS)),
            Some(libc::ELOOP)
        );
        assert!(open_noatime(&dir.path().join("d"), DIR_FLAGS).is_ok());
        assert_eq!(
            errno(open_noatime(&dir.path().join("f"), DIR_FLAGS)),
            Some(libc::ENOTDIR)
        );
        // With O_DIRECTORY set, Linux reports a refused link as ENOTDIR;
        // without O_NOFOLLOW this open would follow the link and succeed.
        assert_eq!(
            errno(open_noatime(&dir.path().join("dlink"), DIR_FLAGS)),
            Some(libc::ENOTDIR)
        );
    }

    #[test]
    fn a_regular_etc_os_release_wins_over_the_fallback() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "etc/os-release", "PRETTY_NAME=\"From etc\"\n");
        write(
            dir.path(),
            "usr/lib/os-release",
            "PRETTY_NAME=\"From usr\"\n",
        );
        let os = inspect(dir.path());
        assert_eq!(os.os_name.as_deref(), Some("From etc"));
        assert!(
            !os.problems.iter().any(|p| p.contains("os-release")),
            "{:?}",
            os.problems
        );
    }

    #[test]
    fn a_fifo_in_place_of_the_log_is_refused_without_blocking() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        let log = dir.path().join("var/log/pacman.log");
        fs::remove_file(&log).unwrap();
        let c = std::ffi::CString::new(log.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // A writer stands ready with an upgrade line, so a read that does open
        // the FIFO gets a date instead of hanging the test.
        let writer = std::thread::spawn({
            let log = log.clone();
            move || {
                let mut f = fs::OpenOptions::new().write(true).open(&log).unwrap();
                let _ = f.write_all(
                    b"[2026-01-01T00:00:00+0000] [PACMAN] starting full system upgrade\n",
                );
            }
        });
        let os = inspect(dir.path());
        // Release the writer when inspect never opened the FIFO.
        let _reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&log)
            .unwrap();
        writer.join().unwrap();
        assert_eq!(
            os.last_full_upgrade_attempted, None,
            "the FIFO must not be read"
        );
        assert!(
            os.problems
                .iter()
                .any(|p| p == "var/log/pacman.log: not a regular file"),
            "{:?}",
            os.problems
        );
    }

    #[test]
    fn packages_are_matched_by_their_name_field_not_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        // The directory looks like linux-cachyos; the package is not.
        add_pkg(
            dir.path(),
            "linux-cachyos-6.0-1",
            "linux-cachyos-lts",
            "6.0-1",
        );
        add_pkg(dir.path(), "odd-dir-name", "btrbk", "0.33.0-1");
        // A desc with no name is skipped, and so is a directory with no desc.
        write(
            dir.path(),
            "var/lib/pacman/local/broken-1-1/desc",
            "%VERSION%\n1-1\n",
        );
        fs::create_dir_all(dir.path().join("var/lib/pacman/local/nodesc-1-1")).unwrap();
        let os = inspect(dir.path());
        assert!(os.packages_read);
        assert_eq!(os.packages.get("linux-cachyos"), None);
        assert_eq!(
            os.packages.get("btrbk").map(String::as_str),
            Some("0.33.0-1")
        );
        assert_eq!(os.packages.len(), 1, "{:?}", os.packages);
        assert!(
            os.problems
                .iter()
                .any(|p| p.starts_with("var/lib/pacman/local/nodesc-1-1/desc: ")),
            "{:?}",
            os.problems
        );
    }

    #[test]
    fn a_desc_without_a_version_is_not_a_version() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "var/lib/pacman/local/btrbk-1/desc",
            "%NAME%\nbtrbk\n\n",
        );
        assert_eq!(inspect(dir.path()).packages.get("btrbk"), None);
    }

    // ---- kernels --------------------------------------------------------

    #[test]
    fn kernel_series_is_major_minor_by_number() {
        assert_eq!(kernel_series("6.12.1-1-cachyos"), Some((6, 12)));
        assert_eq!(kernel_series("7.2"), Some((7, 2)));
        assert_eq!(kernel_series("6.12rc1"), Some((6, 12)));
        assert_eq!(kernel_series("6"), None);
        assert_eq!(kernel_series("6."), None);
        assert_eq!(kernel_series("extramodules-6.12-cachyos"), None);
        assert_eq!(kernel_series(""), None);
    }

    #[test]
    fn the_newest_kernel_is_chosen_numerically() {
        let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let list = k(&["6.6.5-2-lts", "6.12.9-1", "6.12.10-1", "extramodules-6.13"]);
        assert_eq!(newest_kernel(&list).map(String::as_str), Some("6.12.10-1"));
        let list = k(&["6.12.1-1", "6.9.1-1"]);
        assert_eq!(newest_kernel(&list).map(String::as_str), Some("6.12.1-1"));
        let list = k(&["5.15.1", "6.1"]);
        assert_eq!(newest_kernel(&list).map(String::as_str), Some("6.1"));
        assert_eq!(newest_kernel(&k(&["extramodules"])), None);
        assert_eq!(newest_kernel(&[]), None);
    }

    // ---- host -----------------------------------------------------------

    #[test]
    fn host_versions_come_from_uname_and_pacman() {
        let r = Scripted::new(&[
            ("uname -r", 0, "7.2.8-1-cachyos\n"),
            ("pacman -Q btrfs-progs", 0, "btrfs-progs 6.17-1\n"),
        ]);
        assert_eq!(host_versions_with(&r), host("7.2.8-1-cachyos"));
        assert_eq!(r.calls(), ["uname -r", "pacman -Q btrfs-progs"]);
    }

    #[test]
    fn a_failed_or_odd_host_probe_is_unknown() {
        let r = Scripted::new(&[]);
        assert_eq!(host_versions_with(&r), HostVersions::default());
        let r = Scripted::new(&[
            ("uname -r", 0, "\n"),
            ("pacman -Q btrfs-progs", 0, "btrfs-progs-git 6.17-1\n"),
        ]);
        assert_eq!(host_versions_with(&r), HostVersions::default());
        let r = Scripted::new(&[
            ("uname -r", 3, "7.2.8\n"),
            ("pacman -Q btrfs-progs", 1, "btrfs-progs 6.17-1\n"),
        ]);
        assert_eq!(host_versions_with(&r), HostVersions::default());
        let r = Scripted::new(&[("pacman -Q btrfs-progs", 0, "btrfs-progs\n")]);
        assert_eq!(host_versions_with(&r).btrfs_progs, None);
    }

    // ---- assess ---------------------------------------------------------

    #[test]
    fn a_recent_upgrade_on_the_host_series_is_current() {
        let os = os_with(Some(&days_before(TODAY, 1)), &["7.2.1-1-cachyos"]);
        let a = assess(&os, &host("7.2.8-1-cachyos"), TODAY, 60);
        assert_eq!(a.age_days, Some(1));
        assert!(!a.stale, "{:?}", a.reasons);
        assert!(a.reasons.is_empty());
    }

    #[test]
    fn age_is_stale_only_past_the_limit() {
        let h = host("7.2.8");
        let at = |n| {
            assess(
                &os_with(Some(&days_before(TODAY, n)), &["7.2.1"]),
                &h,
                TODAY,
                60,
            )
        };
        assert!(!at(60).stale, "exactly the limit is still current");
        let a = at(61);
        assert!(a.stale);
        assert_eq!(a.age_days, Some(61));
        assert_eq!(a.age_basis, Some(AgeBasis::LastAppliedUpgrade));
        assert_eq!(a.reasons, ["last full upgrade 61 days ago (limit 60)"]);
    }

    #[test]
    fn a_kernel_series_behind_the_host_is_stale_and_ahead_is_not() {
        let up = days_before(TODAY, 3);
        let a = assess(
            &os_with(Some(&up), &["6.12.1-1-cachyos"]),
            &host("7.2.8-1-cachyos"),
            TODAY,
            60,
        );
        assert!(a.stale);
        assert_eq!(
            a.reasons,
            ["kernel series 6.12 (6.12.1-1-cachyos) is behind the host's 7.2"]
        );
        let a = assess(&os_with(Some(&up), &["7.1.9"]), &host("7.2.0"), TODAY, 60);
        assert!(a.stale, "a minor series behind counts");
        let a = assess(&os_with(Some(&up), &["7.3.0"]), &host("7.2.8"), TODAY, 60);
        assert!(!a.stale, "{:?}", a.reasons);
        let a = assess(&os_with(Some(&up), &["8.0.1"]), &host("7.9.1"), TODAY, 60);
        assert!(
            !a.stale,
            "a newer major with a lower minor is ahead: {:?}",
            a.reasons
        );
        // The newest of several kernels is the one compared.
        let a = assess(
            &os_with(Some(&up), &["6.6.1", "7.2.0"]),
            &host("7.2.8"),
            TODAY,
            60,
        );
        assert!(!a.stale, "{:?}", a.reasons);
    }

    #[test]
    fn unknown_is_never_current() {
        let h = host("7.2.8");
        let a = assess(&os_with(None, &["7.2.1"]), &h, TODAY, 60);
        assert!(a.stale);
        assert_eq!(a.age_days, None, "no fabricated age");
        assert_eq!(
            a.reasons,
            ["no full system upgrade recorded in var/log/pacman.log"]
        );

        let up = days_before(TODAY, 1);
        let a = assess(&os_with(Some(&up), &[]), &h, TODAY, 60);
        assert_eq!(
            (a.stale, a.reasons.clone()),
            (true, vec!["no kernel found".to_string()])
        );
        let a = assess(&os_with(Some(&up), &["extramodules"]), &h, TODAY, 60);
        assert_eq!(a.reasons, ["kernel version unknown (extramodules)"]);
        let a = assess(
            &os_with(Some(&up), &["7.2.1"]),
            &HostVersions::default(),
            TODAY,
            60,
        );
        assert_eq!(a.reasons, ["host kernel unknown, kernels not compared"]);
        let odd_host = HostVersions {
            kernel: Some("weird".into()),
            btrfs_progs: None,
        };
        let a = assess(&os_with(Some(&up), &["7.2.1"]), &odd_host, TODAY, 60);
        assert_eq!(a.reasons, ["host kernel unknown, kernels not compared"]);
    }

    #[test]
    fn a_date_after_today_or_an_unreadable_today_is_stale_with_no_age() {
        let h = host("7.2.8");
        let a = assess(&os_with(Some("2026-10-05"), &["7.2.1"]), &h, TODAY, 60);
        assert!(a.stale);
        assert_eq!(a.age_days, None);
        assert_eq!(
            a.reasons,
            ["last full upgrade 2026-10-05 is after today (2026-10-02) — check the clock"]
        );
        let a = assess(&os_with(Some("2026-10-01"), &["7.2.1"]), &h, "garbage", 60);
        assert!(a.stale);
        assert_eq!(a.age_days, None);
        assert_eq!(a.reasons, ["age unknown: today's date is unreadable"]);
        let a = assess(&os_with(Some("2026-10-02"), &["7.2.1"]), &h, TODAY, 60);
        assert_eq!((a.age_days, a.stale), (Some(0), false), "same day is age 0");
    }

    // ---- drives and status ----------------------------------------------

    fn mirror_config(mounts: &[(&str, &str)]) -> Config {
        let mut text = String::from(
            "[general]\nversion = \"0\"\ninstall_prefix = \"/usr\"\ndb_path = \"/x\"\n\
             [init]\nsystem = \"systemd\"\n[schedule]\nincremental = \"03:00\"\nfull = \"Sun 04:00\"\nrandomized_delay_min = 0\n\
             [email]\n[gui]\n\
             [[target]]\nlabel = \"primary\"\nserial = \"P\"\nmount = \"/nonexistent/primary\"\nrole = \"primary\"\n[target.retention]\ndaily = 1\n",
        );
        for (label, mount) in mounts {
            text.push_str(&format!(
                "[[target]]\nlabel = \"{label}\"\nserial = \"S\"\nmount = \"{mount}\"\nrole = \"mirror\"\n[target.retention]\ndaily = 1\n"
            ));
        }
        Config::from_toml(&text).unwrap()
    }

    #[test]
    fn os_root_is_the_at_subvolume_of_the_mount() {
        assert_eq!(os_root("/mnt/r"), PathBuf::from("/mnt/r/@"));
    }

    #[test]
    fn a_drive_whose_root_is_not_a_directory_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let e = inspect_drive("A", &dir.path().join("@"), &host("7.2"), TODAY, 60);
        assert_eq!(e.label, "A");
        assert!(
            matches!(&e.report, DriveReport::Unreadable(m) if m.contains("not a directory")),
            "{e:?}"
        );
        write(dir.path(), "@", "a file, not a root");
        let e = inspect_drive("A", &dir.path().join("@"), &host("7.2"), TODAY, 60);
        assert!(matches!(e.report, DriveReport::Unreadable(_)), "{e:?}");
    }

    #[test]
    fn a_root_that_is_a_symlink_is_refused_not_followed() {
        // `<mount>/@` pointing at `/` would make the host look like the
        // recovery OS: every containment check would pass against `/`.
        let host_like = tempfile::tempdir().unwrap();
        full_root(host_like.path());
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(host_like.path(), dir.path().join("@")).unwrap();
        let e = inspect_drive("A", &dir.path().join("@"), &host("7.2"), TODAY, 60);
        assert!(
            matches!(&e.report, DriveReport::Unreadable(m) if m.contains("is a symlink")),
            "{e:?}"
        );
    }

    #[test]
    fn status_covers_mirror_targets_only_and_inspects_the_mounted_ones() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let c = tempfile::tempdir().unwrap();
        full_root(&a.path().join("@"));
        let cfg = mirror_config(&[
            ("A", a.path().to_str().unwrap()),
            ("B", b.path().to_str().unwrap()),
            ("C", c.path().to_str().unwrap()),
        ]);
        let mounted = [a.path().to_path_buf(), c.path().to_path_buf()];
        let is_mounted = |p: &Path| mounted.iter().any(|m| m == p);
        let entries = status_with(&cfg, &host("7.2.8"), TODAY, &is_mounted);
        let labels: Vec<&str> = entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["A", "B", "C"], "the primary is not a recovery OS");
        match &entries[0].report {
            DriveReport::Inspected { os, assessment } => {
                assert_eq!(os.last_full_upgrade_applied.as_deref(), Some("2026-03-14"));
                assert_eq!(assessment.age_days, Some(202));
                assert!(assessment.stale);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(entries[0].root, a.path().join("@"));
        assert_eq!(entries[1].report, DriveReport::NotMounted);
        assert!(
            matches!(entries[2].report, DriveReport::Unreadable(_)),
            "mounted, no @"
        );
    }

    #[test]
    fn exit_code_ranks_unreadable_over_stale_over_current() {
        let entry = |report| DriveEntry {
            label: "x".into(),
            root: PathBuf::new(),
            report,
        };
        let current = entry(DriveReport::Inspected {
            os: RecoveryOs::default(),
            assessment: Assessment::default(),
        });
        let stale = entry(DriveReport::Inspected {
            os: RecoveryOs::default(),
            assessment: Assessment {
                stale: true,
                ..Default::default()
            },
        });
        let bad = entry(DriveReport::Unreadable("x".into()));
        let off = entry(DriveReport::NotMounted);
        assert_eq!(exit_code(&[]), 0);
        assert_eq!(exit_code(&[current.clone(), off.clone()]), 0);
        assert_eq!(exit_code(&[current.clone(), stale.clone()]), 1);
        assert_eq!(exit_code(&[stale.clone(), bad.clone()]), 2);
        assert_eq!(exit_code(&[bad, stale]), 2);
    }

    // ---- report section -------------------------------------------------

    #[test]
    fn the_section_shows_every_reading_and_the_verdict() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        let h = host("7.2.8-1-cachyos");
        let e = inspect_drive("system-recovery-A-2tb", dir.path(), &h, TODAY, 60);
        let text = format_section(&[e], &h);
        let root = dir.path().display();
        let want = format!(
            "RECOVERY OS\n\
             \x20 system-recovery-A-2tb  ({root})\n\
             \x20   OS                  CachyOS\n\
             \x20   Installed           2026-01-02\n\
             \x20   Last full upgrade   2026-03-14\n\
             \x20   Age                 202 days since last applied upgrade\n\
             \x20   Kernel              6.12.1-1-cachyos (host 7.2.8-1-cachyos)\n\
             \x20   btrfs-progs         6.10-1 (host 6.17-1)\n\
             \x20   btrbk               0.32.6-1\n\
             \x20   das-backup-manager  not installed\n\
             \x20   Result              STALE — last full upgrade 202 days ago (limit 60); \
             kernel series 6.12 (6.12.1-1-cachyos) is behind the host's 7.2; \
             btrfs-progs 6.10-1 is older than the host's 6.17-1\n"
        );
        assert_eq!(text, want);
    }

    #[test]
    fn the_section_says_unknown_never_blank_and_tells_absent_from_unreadable() {
        // Absent: nothing there at all.
        let dir = tempfile::tempdir().unwrap();
        let e = inspect_drive("B", dir.path(), &HostVersions::default(), TODAY, 60);
        let text = format_section(&[e], &HostVersions::default());
        for line in [
            "    OS                  unknown\n",
            "    Installed           unknown\n",
            "    Last full upgrade   unknown\n",
            "    Age                 unknown\n",
            "    Kernel              unknown (host unknown)\n",
            "    btrfs-progs         unknown (host unknown)\n",
            "    btrbk               unknown\n",
            "    das-backup-manager  unknown\n",
            "    Result              STALE — last upgrade unknown: var/log/pacman.log is absent; \
             no kernel found: usr/lib/modules is absent\n",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
        assert!(!text.contains("Could not read"), "{text}");
        assert!(!text.contains(" 0 days"), "{text}");

        // Unreadable: there, but it could not be read — said as such.
        full_root(dir.path());
        fs::remove_file(dir.path().join("var/log/pacman.log")).unwrap();
        fs::create_dir(dir.path().join("var/log/pacman.log")).unwrap();
        let e = inspect_drive("B", dir.path(), &host("6.12.9"), TODAY, 60);
        let text = format_section(&[e], &host("6.12.9"));
        for line in [
            "    Last full upgrade   unknown\n",
            "    Could not read      var/log/pacman.log: not a regular file\n",
            "    Result              STALE — last upgrade unknown: could not read \
             var/log/pacman.log: not a regular file",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
    }

    #[test]
    fn the_section_shows_a_failed_last_attempt_beside_the_applied_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        let log = fs::read_to_string(dir.path().join("var/log/pacman.log")).unwrap();
        write(
            dir.path(),
            "var/log/pacman.log",
            &format!("{log}[2026-10-01T08:00:02+0200] [PACMAN] starting full system upgrade\n"),
        );
        let h = host("6.12.9");
        let text = format_section(&[inspect_drive("A", dir.path(), &h, TODAY, 60)], &h);
        assert!(
            text.contains(
                "    Last full upgrade   2026-03-14\n\
                 \x20   Last attempt        2026-10-01 (did not complete)\n\
                 \x20   Age                 202 days since last applied upgrade\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "last upgrade attempt 2026-10-01 did not complete; last applied 2026-03-14"
            ),
            "{text}"
        );
        // No such row when the last attempt completed.
        let done = tempfile::tempdir().unwrap();
        full_root(done.path());
        let text = format_section(&[inspect_drive("A", done.path(), &h, TODAY, 60)], &h);
        assert!(!text.contains("Last attempt"), "{text}");
    }

    #[test]
    fn the_upgrade_row_says_none_recorded_none_completed_or_unknown() {
        let h = host("7.2.8");
        let row = |os: &RecoveryOs| {
            assert!(assess(os, &h, TODAY, 60).stale);
            upgrade_text(os)
        };
        let mut os = os_with(None, &["7.2.1"]);
        os.last_attempt_completed = false;
        assert_eq!(row(&os), "none recorded", "log read, no upgrade in it");
        os.last_full_upgrade_attempted = Some("2026-10-01".into());
        assert_eq!(row(&os), "none completed", "log read, attempts only");
        os.log_read = false;
        assert_eq!(row(&os), "unknown", "log not read");
        os.last_full_upgrade_attempted = None;
        assert_eq!(row(&os), "unknown");
    }

    #[test]
    fn a_btrfs_progs_pair_that_cannot_be_compared_says_so() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        add_pkg(
            dir.path(),
            "btrfs-progs-6.10-1",
            "btrfs-progs",
            "6.10.r3.gabc-1",
        );
        let h = host("6.12.9");
        let text = format_section(&[inspect_drive("A", dir.path(), &h, TODAY, 60)], &h);
        assert!(
            text.contains("    btrfs-progs         6.10.r3.gabc-1 (host 6.17-1; not compared)\n"),
            "{text}"
        );
    }

    #[test]
    fn the_section_marks_current_unmounted_unreadable_and_no_mirrors() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        write(
            dir.path(),
            "var/log/pacman.log",
            &format!(
                "[{d}T01:00:00+0000] [PACMAN] starting full system upgrade\n\
                 [{d}T01:05:00+0000] [ALPM] transaction completed\n",
                d = days_before(TODAY, 1)
            ),
        );
        add_pkg(dir.path(), "btrfs-progs-6.10-1", "btrfs-progs", "6.17-1");
        let h = host("6.12.3-1-cachyos");
        let current = inspect_drive("A", dir.path(), &h, TODAY, 60);
        let off = DriveEntry {
            label: "B".into(),
            root: PathBuf::from("/mnt/b/@"),
            report: DriveReport::NotMounted,
        };
        let bad = DriveEntry {
            label: "C".into(),
            root: PathBuf::from("/mnt/c/@"),
            report: DriveReport::Unreadable("not a directory".into()),
        };
        let text = format_section(&[current, off, bad], &h);
        assert!(
            text.contains(
                "    Last full upgrade   2026-10-01\n\
                 \x20   Age                 1 day since last applied upgrade\n"
            ),
            "{text}"
        );
        assert!(text.contains("    Result              current\n"), "{text}");
        assert!(text.contains("  B  not mounted\n"), "{text}");
        assert!(
            text.contains("  C  (/mnt/c/@)  UNREADABLE: not a directory\n"),
            "{text}"
        );
        assert_eq!(
            format_section(&[], &h),
            "RECOVERY OS\n  no role = \"mirror\" targets configured\n"
        );
    }

    // ---- state and health -----------------------------------------------

    fn inspected(label: &str, root: &Path) -> DriveEntry {
        inspect_drive(label, root, &host("7.2.8"), TODAY, 60)
    }

    #[test]
    fn epoch_formats_as_utc_minutes() {
        assert_eq!(format_epoch_utc(0), "1970-01-01 00:00 UTC");
        // 2026-10-03 03:20:59 UTC
        let day = day_number("2026-10-03").unwrap();
        assert_eq!(
            format_epoch_utc(day * 86_400 + 3 * 3600 + 20 * 60 + 59),
            "2026-10-03 03:20 UTC"
        );
    }

    #[test]
    fn state_round_trips_keeps_unmounted_drives_and_is_world_readable() {
        let roots = tempfile::tempdir().unwrap();
        full_root(&roots.path().join("a"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery-os.json");
        assert_eq!(
            load_state(&path),
            Ok(None),
            "no file is no record, not an error"
        );

        let a = inspected("A", &roots.path().join("a"));
        let b_bad = DriveEntry {
            label: "B".into(),
            root: PathBuf::from("/mnt/b/@"),
            report: DriveReport::Unreadable("not a directory".into()),
        };
        write_state(&path, &[a.clone(), b_bad], 1000).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        let st = load_state(&path).unwrap().unwrap();
        assert_eq!(st.schema_version, 2);
        let DriveReport::Inspected { os, .. } = &a.report else {
            panic!()
        };
        assert_eq!(
            st.drives["A"],
            StoredDrive {
                checked_epoch: 1000,
                os: Some(os.clone()),
                error: None
            }
        );
        assert_eq!(
            st.drives["B"],
            StoredDrive {
                checked_epoch: 1000,
                os: None,
                error: Some("not a directory".into())
            }
        );

        // Next run: A is not mounted, B is. A's record survives untouched.
        let b = inspected("B", &roots.path().join("a"));
        let a_off = DriveEntry {
            label: "A".into(),
            root: PathBuf::new(),
            report: DriveReport::NotMounted,
        };
        write_state(&path, &[a_off, b], 2000).unwrap();
        let st = load_state(&path).unwrap().unwrap();
        assert_eq!(st.drives["A"].checked_epoch, 1000);
        assert_eq!(st.drives["B"].checked_epoch, 2000);
        assert_eq!(st.drives["B"].error, None);
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["recovery-os.json"], "no temp file left behind");
    }

    #[test]
    fn a_state_that_cannot_be_written_is_an_error_that_names_its_file_once() {
        let roots = tempfile::tempdir().unwrap();
        full_root(&roots.path().join("a"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery-os.json");
        let a = inspected("A", &roots.path().join("a"));
        write_state(&path, std::slice::from_ref(&a), 1000).unwrap();
        crate::fsutil::testing::unreplaceable(&path);

        let err = write_state(&path, &[a], 2000).unwrap_err();

        assert!(
            err.starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert_eq!(
            err.matches(&path.display().to_string()).count(),
            1,
            "the path, once: {err}"
        );
        assert_eq!(
            load_state(&path).unwrap().unwrap().drives["A"].checked_epoch,
            1000
        );
    }

    #[test]
    fn a_corrupt_or_unreadable_state_is_an_error_and_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery-os.json");
        fs::write(&path, "{not json").unwrap();
        assert!(load_state(&path).unwrap_err().contains("recovery-os.json"));
        let e = DriveEntry {
            label: "A".into(),
            root: PathBuf::new(),
            report: DriveReport::NotMounted,
        };
        let err = write_state(&path, &[e], 5).unwrap_err();
        assert!(
            err.contains(&format!(
                "remove it to start over: rm -- '{}'",
                path.display()
            )),
            "{err}"
        );
        assert!(!err.contains('\n'), "one line, for the run report: {err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
        let as_dir = dir.path().join("d");
        fs::create_dir(&as_dir).unwrap();
        assert!(load_state(&as_dir).is_err());
    }

    #[test]
    fn health_reads_live_when_mounted_and_the_stored_record_otherwise() {
        let a = tempfile::tempdir().unwrap();
        full_root(&a.path().join("@"));
        let cfg = mirror_config(&[
            ("A", a.path().to_str().unwrap()),
            ("B", "/nonexistent/b"),
            ("C", "/nonexistent/c"),
            ("D", "/nonexistent/d"),
        ]);
        let mut fresh = os_with(Some(&days_before(TODAY, 2)), &["7.2.1"]);
        fresh.packages.insert("btrfs-progs".into(), "6.17-1".into());
        let checked = day_number("2026-10-01").unwrap() * 86_400 + 3 * 3600 + 20 * 60;
        let mut drives = BTreeMap::new();
        drives.insert(
            "B".into(),
            StoredDrive {
                checked_epoch: checked,
                os: Some(fresh),
                error: None,
            },
        );
        drives.insert(
            "C".into(),
            StoredDrive {
                checked_epoch: checked,
                os: None,
                error: Some("not a directory".into()),
            },
        );
        let state = Ok(Some(StoredState {
            schema_version: STATE_SCHEMA_VERSION,
            drives,
        }));
        let is_mounted = |p: &Path| p == a.path();
        let h = health_with(&cfg, &state, &|| host("7.2.8"), TODAY, &is_mounted);
        assert_eq!(
            h.lines,
            [
                "A (live): installed 2026-01-02, last full upgrade 2026-03-14, age 202 days \
                 since last applied upgrade, kernel 6.12.1-1-cachyos — STALE: last full upgrade 202 days ago (limit 60); kernel series 6.12 \
                 (6.12.1-1-cachyos) is behind the host's 7.2; btrfs-progs 6.10-1 is older \
                 than the host's 6.17-1",
                "B (as of 2026-10-01 03:20 UTC): installed unknown, last full upgrade 2026-09-30, \
                 age 2 days since last applied upgrade, kernel 7.2.1 — current",
                "C (as of 2026-10-01 03:20 UTC): OS root unreadable: not a directory",
                "D: not mounted and never checked",
            ]
        );
        assert_eq!(
            h.warnings,
            [
                "Recovery OS on 'A' is STALE: last full upgrade 202 days ago (limit 60); \
                 kernel series 6.12 (6.12.1-1-cachyos) is behind the host's 7.2; \
                 btrfs-progs 6.10-1 is older than the host's 6.17-1",
                "Recovery OS on 'C' could not be read: not a directory",
                "Recovery OS on 'D' has never been checked",
            ]
        );
    }

    #[test]
    fn health_recomputes_age_from_today_and_reports_an_unreadable_record() {
        let cfg = mirror_config(&[("B", "/nonexistent/b")]);
        let mut drives = BTreeMap::new();
        drives.insert(
            "B".into(),
            StoredDrive {
                checked_epoch: 0,
                os: Some(os_with(Some("2026-08-01"), &["7.2.1"])),
                error: None,
            },
        );
        let state = Ok(Some(StoredState {
            schema_version: STATE_SCHEMA_VERSION,
            drives,
        }));
        let h = health_with(&cfg, &state, &|| host("7.2.8"), TODAY, &|_| false);
        assert_eq!(
            h.warnings,
            ["Recovery OS on 'B' is STALE: last full upgrade 62 days ago (limit 60)"]
        );

        let h = health_with(
            &cfg,
            &Err("bad json".into()),
            &|| host("7.2.8"),
            TODAY,
            &|_| false,
        );
        assert_eq!(
            h.lines,
            ["B: not mounted; stored record unreadable: bad json"]
        );
        assert_eq!(h.warnings, ["Recovery OS state unreadable: bad json"]);

        let h = health_with(&cfg, &Ok(None), &|| host("7.2.8"), TODAY, &|_| false);
        assert_eq!(h.warnings, ["Recovery OS on 'B' has never been checked"]);

        let none = mirror_config(&[]);
        assert_eq!(
            health_with(&none, &Ok(None), &|| host("7.2.8"), TODAY, &|_| false),
            RecoveryHealth::default()
        );
    }

    #[test]
    fn the_health_line_counts_what_could_not_be_read() {
        let cfg = mirror_config(&[("B", "/nonexistent/b")]);
        let mut os = os_with(Some(TODAY), &["7.2.1"]);
        os.problems = vec!["var/log/x: denied".into(), "usr/lib/y: denied".into()];
        let mut drives = BTreeMap::new();
        drives.insert(
            "B".into(),
            StoredDrive {
                checked_epoch: 0,
                os: Some(os.clone()),
                error: None,
            },
        );
        let state = Ok(Some(StoredState {
            schema_version: STATE_SCHEMA_VERSION,
            drives,
        }));
        let h = health_with(&cfg, &state, &|| host("7.2.8"), TODAY, &|_| false);
        assert_eq!(
            h.lines,
            [
                "B (as of 1970-01-01 00:00 UTC): installed unknown, last full upgrade 2026-10-02, \
                 age 0 days since last applied upgrade, kernel 7.2.1 — current; 2 paths unreadable"
            ]
        );
        os.problems.truncate(1);
        assert!(
            summary("B", "live", &os, &Assessment::default())
                .ends_with("— current; 1 path unreadable")
        );
        os.problems.clear();
        assert!(summary("B", "live", &os, &Assessment::default()).ends_with("— current"));
    }

    #[test]
    fn health_probes_the_host_once_and_only_when_there_is_a_mirror() {
        let calls = std::cell::Cell::new(0);
        let probe = || {
            calls.set(calls.get() + 1);
            host("7.2.8")
        };
        let none = mirror_config(&[]);
        health_with(&none, &Ok(None), &probe, TODAY, &|_| false);
        assert_eq!(calls.get(), 0, "no mirror target: no uname, no pacman");
        // Never checked: nothing to compare, so no probe either.
        let two = mirror_config(&[("A", "/nonexistent/a"), ("B", "/nonexistent/b")]);
        health_with(&two, &Ok(None), &probe, TODAY, &|_| false);
        assert_eq!(calls.get(), 0);
        let mut drives = BTreeMap::new();
        for l in ["A", "B"] {
            drives.insert(
                l.to_string(),
                StoredDrive {
                    checked_epoch: 0,
                    os: Some(os_with(Some(TODAY), &["7.2.1"])),
                    error: None,
                },
            );
        }
        let state = Ok(Some(StoredState {
            schema_version: STATE_SCHEMA_VERSION,
            drives,
        }));
        let h = health_with(&two, &state, &probe, TODAY, &|_| false);
        assert_eq!(calls.get(), 1, "two drives, one probe");
        assert!(h.warnings.is_empty(), "{:?}", h.warnings);
    }

    #[test]
    fn the_max_age_from_config_is_the_one_used() {
        let mut cfg = mirror_config(&[("B", "/nonexistent/b")]);
        cfg.recovery_os.max_age_days = 70;
        let mut drives = BTreeMap::new();
        drives.insert(
            "B".into(),
            StoredDrive {
                checked_epoch: 0,
                os: Some(os_with(Some("2026-08-01"), &["7.2.1"])),
                error: None,
            },
        );
        let state = Ok(Some(StoredState {
            schema_version: STATE_SCHEMA_VERSION,
            drives,
        }));
        let h = health_with(&cfg, &state, &|| host("7.2.8"), TODAY, &|_| false);
        assert!(h.warnings.is_empty(), "{:?}", h.warnings);
    }

    // ---- the command cores ----------------------------------------------

    #[test]
    fn status_run_writes_the_state_and_ranks_the_exit_code() {
        let a = tempfile::tempdir().unwrap();
        full_root(&a.path().join("@"));
        let cfg = mirror_config(&[("A", a.path().to_str().unwrap()), ("B", "/nonexistent/b")]);
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("recovery-os.json");
        let is_mounted = |p: &Path| p == a.path();
        let run = status_run_with(&cfg, Some(&state), &host("7.2.8"), TODAY, 77, &is_mounted);
        assert_eq!(run.code, 1, "A is stale");
        assert_eq!(run.state_error, None);
        assert_eq!(run.entries.len(), 2);
        let st = load_state(&state).unwrap().unwrap();
        assert_eq!(st.drives.keys().collect::<Vec<_>>(), ["A"]);
        assert_eq!(st.drives["A"].checked_epoch, 77);

        // Without --state-file nothing is written.
        let other = dir.path().join("none.json");
        let run = status_run_with(&cfg, None, &host("7.2.8"), TODAY, 78, &is_mounted);
        assert_eq!(run.code, 1);
        assert!(!other.exists());
        assert_eq!(
            load_state(&state).unwrap().unwrap().drives["A"].checked_epoch,
            77
        );

        // A state file that cannot be written is exit 2, and says why.
        let bad = dir.path().join("missing-dir/recovery-os.json");
        let run = status_run_with(&cfg, Some(&bad), &host("7.2.8"), TODAY, 79, &|_| false);
        assert_eq!(run.code, 2);
        assert!(run.state_error.unwrap().contains("missing-dir"));
    }

    #[test]
    fn status_run_with_everything_unmounted_is_zero() {
        let cfg = mirror_config(&[("A", "/nonexistent/a")]);
        let run = status_run_with(&cfg, None, &host("7.2.8"), TODAY, 1, &|_| false);
        assert_eq!(
            (run.code, run.entries[0].report.clone()),
            (0, DriveReport::NotMounted)
        );
    }

    #[test]
    fn entry_json_carries_the_state_the_facts_and_the_verdict() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        let e = inspected("A", dir.path());
        let j = entry_json(&e);
        assert_eq!(j["label"], "A");
        assert_eq!(j["root"], dir.path().to_str().unwrap());
        assert_eq!(j["status"], "stale");
        assert_eq!(j["os"]["last_full_upgrade_applied"], "2026-03-14");
        assert_eq!(j["assessment"]["age_days"], 202);
        assert_eq!(j["os"]["installed"], "2026-01-02");
        assert_eq!(j["assessment"]["age_basis"], "last_applied_upgrade");
        assert_eq!(j["error"], serde_json::Value::Null);

        let mut current = e.clone();
        if let DriveReport::Inspected { assessment, .. } = &mut current.report {
            *assessment = Assessment::default();
        }
        assert_eq!(entry_json(&current)["status"], "current");

        let bad = DriveEntry {
            label: "B".into(),
            root: PathBuf::from("/x/@"),
            report: DriveReport::Unreadable("gone".into()),
        };
        let j = entry_json(&bad);
        assert_eq!(
            (j["status"].clone(), j["error"].clone()),
            ("unreadable".into(), "gone".into())
        );
        assert_eq!(j["os"], serde_json::Value::Null);
        let off = DriveEntry {
            label: "C".into(),
            root: PathBuf::from("/y/@"),
            report: DriveReport::NotMounted,
        };
        assert_eq!(entry_json(&off)["status"], "not_mounted");
    }

    #[test]
    fn the_section_and_health_say_never_upgraded_since_install() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        write(dir.path(), "var/log/pacman.log", NEVER_UPGRADED_LOG);
        let h = same_host();
        let e = inspect_drive("A", dir.path(), &h, "2026-10-03", 60);
        let text = format_section(std::slice::from_ref(&e), &h);
        for line in [
            "    Installed           2026-04-12\n",
            "    Last full upgrade   none recorded\n",
            "    Age                 174 days since install\n",
            "    Result              STALE — never upgraded since install on 2026-04-12 (174 days)\n",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
        let j = entry_json(&e);
        assert_eq!(j["os"]["installed"], "2026-04-12");
        assert_eq!(j["assessment"]["age_days"], 174);
        assert_eq!(j["assessment"]["age_basis"], "install");
        let DriveReport::Inspected { os, assessment } = &e.report else {
            panic!("{e:?}")
        };
        assert_eq!(
            summary("A", "live", os, assessment),
            "A (live): installed 2026-04-12, last full upgrade none recorded, \
             age 174 days since install, kernel 6.12.1-1-cachyos — STALE: never upgraded \
             since install on 2026-04-12 (174 days)"
        );
        // No age: said as unknown, never as a number, and JSON carries null.
        let unknown = upgrades("garbage\n");
        let a = assess(&unknown, &h, "2026-10-03", 60);
        assert_eq!(age_text(&a), "unknown");
        assert_eq!(
            serde_json::to_value(&a).unwrap()["age_basis"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn the_kernel_row_names_the_host_kernel_whichever_is_newer() {
        let dir = tempfile::tempdir().unwrap();
        full_root(dir.path());
        fs::create_dir_all(dir.path().join("usr/lib/modules/7.3.0-1-cachyos")).unwrap();
        let newer_host = host("7.4.1-1-cachyos");
        let older_host = host("7.2.8-1-cachyos");
        for (h, want) in [
            (
                &newer_host,
                "    Kernel              7.3.0-1-cachyos (host 7.4.1-1-cachyos)\n",
            ),
            (
                &older_host,
                "    Kernel              7.3.0-1-cachyos (host 7.2.8-1-cachyos)\n",
            ),
        ] {
            let text = format_section(&[inspect_drive("A", dir.path(), h, TODAY, 60)], h);
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
    }

    #[test]
    fn a_state_of_another_schema_version_is_refused_with_the_way_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery-os.json");
        let hint = format!("remove it to start over: rm -- '{}'", path.display());
        let v1 = r#"{"schema_version":1,"drives":{}}"#;
        fs::write(&path, v1).unwrap();
        let err = load_state(&path).unwrap_err();
        assert_eq!(
            err,
            format!(
                "{}: record schema version 1, this btrdasd reads 2 — left as it is; {hint}",
                path.display()
            )
        );
        // The writer refuses it the same way and leaves it alone.
        let off = DriveEntry {
            label: "A".into(),
            root: PathBuf::new(),
            report: DriveReport::NotMounted,
        };
        assert_eq!(write_state(&path, &[off], 5).unwrap_err(), err);
        assert_eq!(fs::read_to_string(&path).unwrap(), v1);
        // No version at all, or a newer one, is refused too.
        fs::write(&path, r#"{"drives":{}}"#).unwrap();
        assert!(
            load_state(&path)
                .unwrap_err()
                .contains(": record schema version none, this btrdasd reads 2 — "),
            "{:?}",
            load_state(&path)
        );
        fs::write(&path, r#"{"schema_version":3,"drives":{}}"#).unwrap();
        assert!(load_state(&path).unwrap_err().ends_with(&hint));
        // The current version loads.
        fs::write(&path, r#"{"schema_version":2,"drives":{}}"#).unwrap();
        assert_eq!(
            load_state(&path),
            Ok(Some(StoredState {
                schema_version: 2,
                drives: BTreeMap::new()
            }))
        );
        // A corrupt file says the same way out from the loader itself.
        fs::write(&path, "{not json").unwrap();
        assert!(load_state(&path).unwrap_err().ends_with(&hint));
    }
}
