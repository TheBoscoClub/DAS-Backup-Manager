//! Whether booting a recovery OS would run btrbk (bd DAS-Backup-Manager-1yg)
//! — on bare metal or in the update VM — and when.
//!
//! A walk starts where systemd starts: `default.target`
//! (`etc/systemd/system/default.target`, `systemctl set-default`'s link,
//! else the vendor's), and every unit that pulls in, labelled "at every
//! boot". It goes on from each unit to what it starts: the units its
//! `[Unit]` section pulls in (`Wants=`, `Requires=`, `Requisite=`,
//! `BindsTo=`, `Upholds=`, `OnSuccess=`, `OnFailure=`), the names in its
//! `.wants/`, `.requires/` and `.upholds/` directories, the unit a timer or
//! a path unit names, and the service a socket starts when something
//! connects to it. Then it does the same from every name in every
//! dependency directory of the unit trees — what `systemctl enable` and
//! packages enable — for what the boot did not reach. Each unit is read
//! once by name, in systemd's precedence order (an instance falls back to
//! its template), with its drop-ins; a unit reached again sooner than
//! before passes that on; at most [`MAX_UNITS`] are read.
//!
//! A unit runs btrbk if a command it runs names it — in an `Exec…=` line,
//! in a script that command runs, in a script that script runs, at most
//! [`MAX_SCRIPT_DEPTH`] scripts deep — or runs a program that is btrbk under
//! another name. Programs are read where they would run: an absolute path,
//! a bare name where systemd or a shell looks for it, the script a shell or
//! an interpreter is given, the command a wrapper (`env`, `nice`, `flock`,
//! `timeout`, `sudo`…) runs. A word that only contains btrbk (`btrbk.sh`,
//! `run-btrbk`) means it may. When a cron daemon is among the units, its
//! tables and scripts are read the same way. Each runner's config is the
//! `-c` it passes, else btrbk's default; it is only `lstat`ed.
//!
//! Everything is read with the parent module's rules: `O_NOATIME`, absent
//! told from unreadable, and nothing written. A link is never read on a
//! mount that would record the access — `readlinkat` updates the link's
//! access time, and no flag prevents it — so links are followed only where
//! `fstatvfs` says the mount is `noatime` or read-only, as every DAS target
//! is mounted. There a link resolves inside the root (an absolute target is
//! the root's, `..` cannot leave it, at most [`MAX_LINKS`] links); a link
//! to `/dev/null`, or an empty unit file, is masked and runs nothing; a
//! link to another unit's file is an alias for that unit. Elsewhere a link
//! is not read, and then:
//!
//! - a unit file linked into `/etc` cannot be told from one masked there:
//!   either may run btrbk; a package's link among the vendor units is its
//!   alias and is skipped, unless its name is btrbk's;
//! - a program named through a link (`/usr/bin/sh` → `bash`) is not looked
//!   into, and a path through one of Arch's merged-`/usr` links is read
//!   where Arch's `filesystem` package points it ([`MERGED_USR`]).
//!
//! The root read is that OS's `@` subvolume alone. A program, script or
//! config under a mount point of its `etc/fstab` or of a `.mount` unit
//! (CachyOS mounts `/root`, `/home`, `/srv` from subvolumes of their own) is
//! not the one it will see, so it is unknown; so is a program under `/run`,
//! which only a running system fills. A plain argument that is not there is
//! no program, and is ignored.
//!
//! Not read: a script named through a variable, a program in an option's
//! value, a program behind a wrapper this does not know, units generators
//! create at boot, units udev, D-Bus or mounts start, user units and user
//! managers, and `/etc/rc.local`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::{
    BTRBK_CONFIGS, BootVerdict, BtrbkAtBoot, BtrbkConfig, BtrbkRunner, DIR_FLAGS, EnabledUnit,
    EnabledUnits, MountFlags, ReadErr, dir_names_noatime, io_error, open_error, open_in_root,
    open_noatime, read_in_root, read_link_at, records_access, resolve_in_root,
};

/// The persistent unit trees, highest precedence first: systemd 262's unit
/// path (`systemd-analyze unit-paths`) less the ones under /run, which are
/// empty at rest. `system.control` holds `systemctl set-property`'s
/// drop-ins, `system.attached` portable services' units.
pub(super) const UNIT_TREES: [&str; 5] = [
    "etc/systemd/system.control",
    "etc/systemd/system",
    "etc/systemd/system.attached",
    "usr/local/lib/systemd/system",
    "usr/lib/systemd/system",
];
/// The tree packages own.
const VENDOR_TREE: &str = "usr/lib/systemd/system";
/// The dependency directories `systemctl enable` fills from `WantedBy=`,
/// `RequiredBy=` and `UpheldBy=` (systemd.unit(5), `[Install]`).
const DEPENDENCY_DIRS: [&str; 3] = [".wants", ".requires", ".upholds"];
/// The `[Unit]` settings that start another unit with this one, or when it
/// ends (systemd.unit(5)). An empty one resets nothing: "Dependencies
/// (After=, etc.) cannot be reset to an empty list" — systemd 262's own
/// parser agrees.
const PULLS: [&str; 7] = [
    "Wants",
    "Requires",
    "Requisite",
    "BindsTo",
    "Upholds",
    "OnSuccess",
    "OnFailure",
];
/// The unit types whose files this reads for what they run or start.
const STARTERS: [&str; 4] = ["service", "socket", "timer", "path"];
/// systemd's unit types: a link to a file named with one is another unit's.
const UNIT_TYPES: [&str; 11] = [
    "service",
    "socket",
    "target",
    "timer",
    "path",
    "mount",
    "automount",
    "swap",
    "slice",
    "scope",
    "device",
];
/// What systemd starts at boot (systemd.special(7)).
const DEFAULT_TARGET: &str = "default.target";
/// The most units the walk reads. The host this was written on starts 413
/// from 272 enabled names; past the cap the rest are unknown.
const MAX_UNITS: usize = 4096;
/// How many starts the walk follows, at most. Each unit is read once and
/// expanded again only when reached sooner — at most five times more, for
/// six ranks of [`When`] — so a real system stays far below this; it ends
/// the walk by itself, whatever befalls that rule.
const MAX_PULLS: usize = MAX_UNITS * 64;
/// Targets a system reaches only to stop, sleep, or start from its initrd:
/// a unit only they pull in does not run at boot. Matched as name prefixes.
const NOT_AT_BOOT: [&str; 14] = [
    "shutdown.",
    "reboot.",
    "halt.",
    "poweroff.",
    "kexec.",
    "final.",
    "sleep.",
    "suspend",
    "hibernate.",
    "hybrid-sleep.",
    "initrd",
    "emergency.",
    "rescue.",
    "factory-reset.",
];
/// The units that run cron jobs.
const CRON_UNITS: [&str; 7] = [
    "cronie.service",
    "crond.service",
    "cron.service",
    "dcron.service",
    "fcron.service",
    "anacron.service",
    "anacron.timer",
];
const ANACRONTAB: &str = "etc/anacrontab";
/// Cron's tables that name the user each job runs as, and anacron's.
const CRON_FILES: [&str; 2] = ["etc/crontab", ANACRONTAB];
const CRON_DIRS: [&str; 7] = [
    "etc/cron.d",
    "etc/cron.hourly",
    "etc/cron.daily",
    "etc/cron.weekly",
    "etc/cron.monthly",
    "var/spool/cron",
    "var/spool/cron/crontabs",
];
/// Directories of scripts rather than tables: run-parts runs each file.
const CRON_SCRIPT_DIRS: [&str; 4] = [
    "etc/cron.hourly",
    "etc/cron.daily",
    "etc/cron.weekly",
    "etc/cron.monthly",
];
/// The script directories anacron runs, when its table names them.
const ANACRON_DIRS: [&str; 3] = ["cron.daily", "cron.weekly", "cron.monthly"];
/// Arch's merged-`/usr` links, as its `filesystem` package makes them: where
/// a link cannot be read, a path through one is read where it leads on
/// Arch, taken by name. Nothing there is unknown, not absent: on another
/// layout the link may lead elsewhere.
const MERGED_USR: [(&str, &str); 6] = [
    ("bin", "/usr/bin"),
    ("sbin", "/usr/bin"),
    ("lib", "/usr/lib"),
    ("lib64", "/usr/lib"),
    ("usr/sbin", "/usr/bin"),
    ("usr/lib64", "/usr/lib"),
];
/// What systemd mounts from memory before anything runs — `/run` (a tmpfs
/// `/var/run` and `/var/lock` lead into), `/dev`, `/proc` and `/sys` — over
/// whatever the disk holds there. A plain argument there is ignored; a
/// program there is unknown: something at boot may put it there.
const VOLATILE: [&str; 6] = ["run", "var/run", "var/lock", "dev", "proc", "sys"];
const FSTAB: &str = "etc/fstab";
/// Filesystems that live in memory: what the disk holds under an fstab
/// mount point of one of these is hidden, not elsewhere.
const MEMORY_FS: [&str; 6] = ["tmpfs", "ramfs", "proc", "sysfs", "devtmpfs", "devpts"];
/// A file this reads that is larger than this is unknown, not read in part.
const MAX_READ_BYTES: u64 = 64 * 1024;
/// The words that end one shell command.
const OPERATORS: [&str; 6] = [";", "&", "|", "(", ")", "`"];
const BTRBK: &str = "btrbk";
/// How many links one path may pass through before it is unknown.
const MAX_LINKS: usize = 8;
/// How many scripts deep a command is read: the script it runs is the
/// first; one past the last is unknown.
const MAX_SCRIPT_DEPTH: usize = 4;
/// Where systemd looks for a program named without a path, on a system
/// that merged sbin into bin as Arch has: systemd.service(5), "Command
/// lines" — "/usr/local/bin/, /usr/bin/, and their sbin/ counterparts (only
/// on systems using split bin/ and sbin/)"; `systemd-path
/// search-binaries-default` on Arch prints `/usr/local/bin:/usr/bin`.
const MERGED_PATH: [&str; 2] = ["usr/local/bin", "usr/bin"];
/// The same on a system that keeps sbin apart (its `usr/sbin` a directory).
const SPLIT_PATH: [&str; 4] = ["usr/local/sbin", "usr/local/bin", "usr/sbin", "usr/bin"];
/// Shells: `-c` passes them a command line; else their first operand is a
/// script they run, `#!` line or not. `.` and `source` run one too.
const SHELLS: [&str; 10] = [
    "sh", "bash", "dash", "zsh", "ksh", "mksh", "ash", "yash", ".", "source",
];
/// Interpreters: their first operand is a script they run.
const INTERPRETERS: [&str; 8] = [
    "python", "python2", "python3", "perl", "ruby", "node", "php", "lua",
];
/// An interpreter's options that run code or a module rather than a file.
const INTERPRETER_CODE: [&str; 5] = ["-c", "-e", "-E", "--eval", "-m"];
/// An interpreter's options that take the next word as their value.
const INTERPRETER_VALUED: [&str; 6] = ["-W", "-X", "-I", "-M", "-r", "--require"];
/// Shell keywords: the word after one is still in command position.
const KEYWORDS: [&str; 15] = [
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "esac", "!", "{", "}",
    "time", "coproc",
];
/// Shell keywords that start what is not a command up to the next operator.
const HEADERS: [&str; 4] = ["for", "select", "case", "function"];

/// A program that runs a command given after its own options.
struct Wrapper {
    name: &'static str,
    /// Its options that take the next word as their value.
    valued: &'static [&'static str],
    /// How many operands it takes before the command.
    operands: usize,
    /// Its options whose value is a whole command line.
    command: &'static [&'static str],
}

/// The wrappers this knows. A program behind any other is not read.
const WRAPPERS: [Wrapper; 25] = [
    Wrapper {
        name: "env",
        valued: &["-u", "--unset", "-C", "--chdir"],
        operands: 0,
        command: &["-S", "--split-string"],
    },
    Wrapper {
        name: "nice",
        valued: &["-n", "--adjustment"],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "ionice",
        valued: &[
            "-c",
            "--class",
            "-n",
            "--classdata",
            "-p",
            "--pid",
            "-P",
            "--pgid",
            "-u",
            "--uid",
        ],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "chrt",
        valued: &[
            "-T",
            "--sched-runtime",
            "-P",
            "--sched-period",
            "-D",
            "--sched-deadline",
        ],
        operands: 1,
        command: &[],
    },
    Wrapper {
        name: "taskset",
        valued: &[],
        operands: 1,
        command: &[],
    },
    Wrapper {
        name: "flock",
        valued: &["-w", "--timeout", "-E", "--conflict-exit-code"],
        operands: 1,
        command: &["-c", "--command"],
    },
    Wrapper {
        name: "timeout",
        valued: &["-s", "--signal", "-k", "--kill-after"],
        operands: 1,
        command: &[],
    },
    Wrapper {
        name: "nohup",
        valued: &[],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "setsid",
        valued: &[],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "stdbuf",
        valued: &["-i", "-o", "-e"],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "time",
        valued: &["-f", "--format", "-o", "--output"],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "exec",
        valued: &["-a"],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "sudo",
        valued: &[
            "-u",
            "--user",
            "-g",
            "--group",
            "-C",
            "--close-from",
            "-D",
            "--chdir",
            "-h",
            "--host",
            "-p",
            "--prompt",
            "-r",
            "--role",
            "-t",
            "--type",
            "-T",
            "--command-timeout",
            "-U",
            "--other-user",
        ],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "doas",
        valued: &["-u", "-C"],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "runuser",
        valued: &[
            "-u",
            "--user",
            "-g",
            "--group",
            "-G",
            "--supp-group",
            "-w",
            "--whitelist-environment",
            "-s",
            "--shell",
        ],
        operands: 0,
        command: &["-c", "--command"],
    },
    Wrapper {
        name: "su",
        valued: &[
            "-s",
            "--shell",
            "-g",
            "--group",
            "-G",
            "--supp-group",
            "-w",
            "--whitelist-environment",
        ],
        operands: 1,
        command: &["-c", "--command"],
    },
    Wrapper {
        name: "xargs",
        valued: &[
            "-a",
            "--arg-file",
            "-d",
            "--delimiter",
            "-E",
            "-I",
            "-L",
            "-n",
            "--max-args",
            "-P",
            "--max-procs",
            "-s",
            "--max-chars",
        ],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "systemd-run",
        valued: &[
            "-u",
            "--unit",
            "-p",
            "--property",
            "-E",
            "--setenv",
            "-H",
            "--host",
            "-M",
            "--machine",
            "--description",
            "--slice",
            "--uid",
            "--gid",
            "--nice",
            "--working-directory",
            "--on-active",
            "--on-boot",
            "--on-startup",
            "--on-unit-active",
            "--on-unit-inactive",
            "--on-calendar",
            "--timer-property",
            "--path-property",
            "--socket-property",
        ],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "systemd-inhibit",
        valued: &["--what", "--who", "--why", "--mode"],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "systemd-cat",
        valued: &[
            "-t",
            "--identifier",
            "-p",
            "--priority",
            "--stderr-priority",
        ],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "setpriv",
        valued: &[],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "unshare",
        valued: &[],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "nsenter",
        valued: &["-t", "--target", "-S", "--setuid", "-G", "--setgid"],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "cgexec",
        valued: &["-g"],
        operands: 0,
        command: &[],
    },
    Wrapper {
        name: "dbus-run-session",
        valued: &[],
        operands: 0,
        command: &[],
    },
];

/// One persistent unit directory's entries.
enum Listing {
    Absent,
    Names(BTreeSet<String>),
    Unreadable(String),
}

/// A unit tree, listed once and consulted for every lookup in it.
struct Tree {
    rel: &'static str,
    listing: Listing,
}

/// The entry names of the directory `rel`, read through
/// [`dir_names_noatime`].
fn list_dir(root: &Path, rel: &str) -> Listing {
    let path = match resolve_in_root(root, rel) {
        Ok(path) => path,
        Err(ReadErr::Absent) => return Listing::Absent,
        Err(ReadErr::Unreadable(why)) => return Listing::Unreadable(why),
    };
    match dir_names_noatime(&path) {
        Ok(names) => Listing::Names(names.into_iter().collect()),
        Err(e) => match io_error(rel, &e) {
            ReadErr::Absent => Listing::Absent,
            ReadErr::Unreadable(why) => Listing::Unreadable(why),
        },
    }
}

/// What one entry is, by its own `lstat`: never followed, never opened. Its
/// parent is resolved by [`resolve_in_root`], which refuses any symlink on
/// the way.
enum Entry {
    File,
    Dir,
    Symlink(String),
    Other,
    Absent,
    Unreadable(String),
}

fn entry_at(root: &Path, rel: &str) -> Entry {
    let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    if matches!(name, "" | "." | "..") {
        return Entry::Unreadable(format!("{rel}: not a plain relative path"));
    }
    let dir = match resolve_in_root(root, parent) {
        Ok(dir) => dir,
        Err(ReadErr::Absent) => return Entry::Absent,
        Err(ReadErr::Unreadable(why)) => return Entry::Unreadable(why),
    };
    let path = dir.join(name);
    match fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_symlink() => Entry::Symlink(format!(
            "{rel}: {} is a symlink, not followed",
            path.display()
        )),
        Ok(m) if m.is_file() => Entry::File,
        Ok(m) if m.is_dir() => Entry::Dir,
        Ok(_) => Entry::Other,
        Err(e) => match io_error(rel, &e) {
            ReadErr::Absent => Entry::Absent,
            ReadErr::Unreadable(why) => Entry::Unreadable(why),
        },
    }
}

/// Why a link was not read.
enum LinkErr {
    /// Its mount would record the access ([`records_access`]): not read.
    Refused(String),
    /// It could not be read.
    Failed(String),
}

/// The target of the link `rel`, whose parent holds no link: read with
/// `readlinkat` on its directory, opened `O_NOATIME`, only where `probe`
/// says that mount records no access.
fn link_target(root: &Path, rel: &str, probe: MountFlags) -> Result<String, LinkErr> {
    let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let dir = resolve_in_root(root, parent)
        .map_err(|e| match e {
            ReadErr::Absent => LinkErr::Failed(format!("{rel}: listed, then gone")),
            ReadErr::Unreadable(why) => LinkErr::Failed(why),
        })
        .and_then(|path| {
            open_noatime(&path, DIR_FLAGS).map_err(|e| LinkErr::Failed(open_error(parent, &e)))
        })?;
    if records_access(probe(&dir)) {
        return Err(LinkErr::Refused(format!(
            "{rel}: a link, not read: this mount records access times (mount it noatime)"
        )));
    }
    read_link_at(&dir, name).map_err(|e| LinkErr::Failed(format!("{rel}: {e}")))
}

/// What a path under the root is once the links on it are followed —
/// those this may read ([`link_target`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Located {
    /// A regular file: where every link led, and its size.
    File(String, u64),
    Dir(String),
    /// A FIFO, a device or a socket.
    Node(String),
    /// Nothing there; `mapped` names the merged-/usr link taken by name on
    /// the way, which may lead elsewhere on another layout.
    Absent {
        rel: String,
        mapped: Option<&'static str>,
    },
    /// A link to `/dev/null`.
    Masked,
    /// A link was not read, its mount recording access times; `last` when
    /// nothing followed it in the path (not even a trailing `/`).
    Unread {
        why: String,
        last: bool,
    },
    /// It could not be told: a link out of the root or through more than
    /// [`MAX_LINKS`] links, a component that could not be read.
    Unknown(String),
}

/// Follow `rel` component by component with `lstat`, reading each link with
/// [`link_target`]: an absolute target is the root's, a relative one its
/// directory's; `..` may not leave the root. A link that cannot be read is
/// taken by name if it is one of Arch's [`MERGED_USR`], else
/// [`Located::Unread`].
fn locate(root: &Path, rel: &str, probe: MountFlags) -> Located {
    let mut done: Vec<String> = Vec::new();
    let mut todo: VecDeque<String> = rel.split('/').map(String::from).collect();
    let mut links = 0..MAX_LINKS;
    let mut mapped = None;
    while let Some(name) = todo.pop_front() {
        match name.as_str() {
            "" | "." => continue,
            ".." => {
                if done.pop().is_none() {
                    return Located::Unknown(format!("{rel}: leads out of the root"));
                }
                continue;
            }
            _ => done.push(name),
        }
        let here = done.join("/");
        let meta = match fs::symlink_metadata(root.join(&here)) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return absent(done, todo, mapped);
            }
            Err(e) => return Located::Unknown(open_error(&here, &e)),
        };
        if meta.file_type().is_symlink() {
            if links.next().is_none() {
                return Located::Unknown(format!("{rel}: more than {MAX_LINKS} links"));
            }
            let target = match link_target(root, &here, probe) {
                Ok(target) => target,
                Err(LinkErr::Refused(why)) => match MERGED_USR.iter().find(|(l, _)| *l == here) {
                    Some((link, to)) => {
                        mapped = Some(*link);
                        to.to_string()
                    }
                    None => {
                        return Located::Unread {
                            why,
                            last: todo.is_empty(),
                        };
                    }
                },
                Err(LinkErr::Failed(why)) => return Located::Unknown(why),
            };
            done.pop();
            if target.starts_with('/') {
                done.clear();
            }
            if target.trim_end_matches('/') == "/dev/null" {
                return Located::Masked;
            }
            for part in target.split('/').rev() {
                todo.push_front(part.to_string());
            }
            continue;
        }
        // Anything after it, a trailing `/` or `.` included, needs a
        // directory, as path resolution does (ENOTDIR).
        let more = !todo.is_empty();
        if more && !meta.is_dir() {
            // A file where a directory would be: nothing is under it.
            return absent(done, todo, mapped);
        }
        if !more {
            return if meta.is_file() {
                Located::File(here, meta.len())
            } else if meta.is_dir() {
                Located::Dir(here)
            } else {
                Located::Node(here)
            };
        }
    }
    Located::Dir(done.join("/"))
}

fn absent(done: Vec<String>, todo: VecDeque<String>, mapped: Option<&'static str>) -> Located {
    let rel: Vec<String> = done
        .into_iter()
        .chain(todo)
        .filter(|c| !c.is_empty() && c != ".")
        .collect();
    Located::Absent {
        rel: rel.join("/"),
        mapped,
    }
}

/// A file read for this analysis.
enum FileRead {
    Text(String),
    /// Nothing there, or a link to `/dev/null`.
    Nothing,
    /// A directory, a FIFO, a device: not a file anything here reads.
    NotFile,
    /// A link not read (see [`Located::Unread`]).
    Unread(String),
    Unknown(String),
}

/// Read `rel` through its links ([`locate`]) with [`read_in_root`]
/// (`O_NOATIME`), unless it is larger than [`MAX_READ_BYTES`]: that is
/// unknown, not read in part.
fn read_file(root: &Path, rel: &str, probe: MountFlags) -> FileRead {
    match locate(root, rel, probe) {
        Located::File(_, len) if len > MAX_READ_BYTES => {
            FileRead::Unknown(format!("{rel}: larger than 64 KiB, not read"))
        }
        Located::File(at, _) => match read_in_root(root, &at) {
            Ok(text) => FileRead::Text(text),
            Err(ReadErr::Absent) => FileRead::Nothing,
            Err(ReadErr::Unreadable(why)) => FileRead::Unknown(why),
        },
        Located::Absent { .. } | Located::Masked => FileRead::Nothing,
        Located::Dir(_) | Located::Node(_) => FileRead::NotFile,
        Located::Unread { why, .. } => FileRead::Unread(why),
        Located::Unknown(why) => FileRead::Unknown(why),
    }
}

/// Whether `rel` is `dir` or under it.
fn under(rel: &str, dir: &str) -> bool {
    rel.strip_prefix(dir)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Where that OS mounts other filesystems over its root at boot. The root
/// read here is its `@` subvolume alone, so what is at or under such a mount
/// point is not what that OS will see there.
struct Mounts {
    /// Each mount point relative to the root, and what mounts it.
    points: Vec<(String, String)>,
    /// Why its fstab could not be read, if it could not.
    fstab: Option<String>,
}

impl Mounts {
    /// Why `rel` may not be what that OS sees: it is at or under a mount
    /// point, whose filesystem shadows the disk's copy at boot.
    fn covers(&self, rel: &str) -> Option<String> {
        self.points
            .iter()
            .find(|(point, _)| under(rel, point))
            .map(|(point, by)| {
                format!("{rel}: under /{point}, which that OS mounts from elsewhere ({by})")
            })
    }

    /// Why `rel`, not here, may be there once that OS is up: it is under a
    /// mount point, or its fstab could not be read.
    fn hides(&self, rel: &str) -> Option<String> {
        self.covers(rel).or_else(|| {
            self.fstab.as_ref().map(|why| {
                format!("{rel}: not here, and what that OS mounts over its root is unknown: {why}")
            })
        })
    }
}

/// The mount points of an fstab (fstab(5): the second field, with `\040`
/// for a space and `\011` for a tab), relative to the root, but `/` and the
/// ones a [`MEMORY_FS`] filesystem is mounted on.
fn fstab_points(text: &str) -> Vec<String> {
    let mut points = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(spec), Some(point)) = (fields.next(), fields.next()) else {
            continue;
        };
        if spec.starts_with('#') || MEMORY_FS.contains(&fields.next().unwrap_or_default()) {
            continue;
        }
        // `none` or `swap` for a swap area: no mount point.
        let Some(rel) = point.strip_prefix('/') else {
            continue;
        };
        let rel = rel.replace("\\040", " ").replace("\\011", "\t");
        let rel = rel.trim_end_matches('/');
        if !rel.is_empty() {
            points.push(rel.to_string());
        }
    }
    points
}

/// The path a `.mount` or `.automount` unit's name stands for, which
/// systemd requires it to be named after (systemd.mount(5)): `-` for `/`,
/// `\xNN` for a byte. `None` for `-.mount`, the root.
fn unit_mount_point(name: &str) -> Option<String> {
    let stem = stem_of(name);
    let mut bytes = Vec::new();
    let mut rest = stem.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        match (b, tail) {
            (b'\\', [b'x', hi, lo, tail @ ..]) => {
                match u8::from_str_radix(std::str::from_utf8(&[*hi, *lo]).ok()?, 16) {
                    Ok(byte) => bytes.push(byte),
                    Err(_) => return None,
                }
                rest = tail;
            }
            (b'-', _) => {
                bytes.push(b'/');
                rest = tail;
            }
            _ => {
                bytes.push(b);
                rest = tail;
            }
        }
    }
    let path = String::from_utf8(bytes).ok()?;
    let path = path.trim_matches('/');
    (!path.is_empty()).then(|| path.to_string())
}

/// That OS's [`Mounts`]: its `etc/fstab`, and every `.mount` or
/// `.automount` unit its trees hold or enable but the memory filesystems'
/// (a mount unit's `Type=`) and those under [`VOLATILE`].
fn read_mounts(root: &Path, trees: &[Tree], enabled: &EnabledUnits, probe: MountFlags) -> Mounts {
    let (listed, fstab) = match read_file(root, FSTAB, probe) {
        FileRead::Text(text) => (fstab_points(&text), None),
        FileRead::Nothing => (Vec::new(), None),
        FileRead::NotFile => (Vec::new(), Some(format!("{FSTAB}: not a regular file"))),
        FileRead::Unread(why) | FileRead::Unknown(why) => (Vec::new(), Some(why)),
    };
    let mut points: Vec<(String, String)> =
        listed.into_iter().map(|p| (p, FSTAB.to_string())).collect();
    let mut names: BTreeSet<String> = BTreeSet::new();
    for tree in trees {
        if let Listing::Names(listed) = &tree.listing {
            names.extend(listed.iter().cloned());
        }
    }
    if let EnabledUnits::Listed { units } = enabled {
        names.extend(units.iter().map(|u| u.name.clone()));
    }
    for name in names {
        let kind = unit_type(&name);
        if kind != "mount" && kind != "automount" {
            continue;
        }
        let Some(point) = unit_mount_point(&name) else {
            continue;
        };
        if VOLATILE.iter().any(|v| under(&point, v)) {
            continue;
        }
        if kind == "mount"
            && let Loaded::Settings(settings) = load_unit(root, trees, &name, &[], probe)
            && settings
                .mount_type
                .as_deref()
                .is_some_and(|t| MEMORY_FS.contains(&t))
        {
            continue;
        }
        points.push((point, name));
    }
    Mounts { points, fstab }
}

/// What an absolute path a command names is, for whether it runs btrbk.
enum Program {
    /// A script, read whole.
    Script(String),
    /// btrbk itself, named through a link.
    Btrbk,
    /// Nothing this reads: a binary, a file with no `#!` line, a directory,
    /// a node, nothing at all.
    Other,
    /// It could not be told.
    Unknown(String),
}

/// How a path a command names is run, which decides what an unknown means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The program a command runs.
    Program,
    /// The script a shell or an interpreter is given: read even with no `#!`
    /// line, as the shell runs it anyway.
    Script,
    /// Any other word: read only if it is a `#!` script that is there.
    Argument,
}

/// `path`, an absolute path a command names, run as `role`. What that OS
/// will not see as it is here — under [`VOLATILE`], under a mount point,
/// not here where a merged-/usr link was taken by name, through a link
/// that could not be read — is unknown for a program and ignored for an
/// argument; a program link not read is not looked into (declared).
fn program_at(root: &Path, mounts: &Mounts, probe: MountFlags, path: &str, role: Role) -> Program {
    let rel = path.trim_matches('/');
    let unknown = |why: String| match role {
        Role::Argument => Program::Other,
        Role::Program | Role::Script => Program::Unknown(why),
    };
    // `/`, and `/etc/` written as a directory: directories, whatever is there.
    if rel.is_empty() {
        return Program::Other;
    }
    if let Some(dir) = VOLATILE.iter().find(|v| under(rel, v)) {
        return unknown(format!(
            "{path}: under /{dir}, which only a running system fills"
        ));
    }
    if let Some(why) = mounts.covers(rel) {
        return unknown(why);
    }
    match locate(root, rel, probe) {
        Located::File(at, _) => {
            if let Some(dir) = VOLATILE.iter().find(|v| under(&at, v)) {
                return unknown(format!(
                    "{path}: leads under /{dir}, which only a running system fills"
                ));
            }
            if let Some(why) = mounts.covers(&at) {
                return unknown(why);
            }
            if role != Role::Argument && at.rsplit('/').next() == Some(BTRBK) {
                return Program::Btrbk;
            }
            read_script(root, &at, role)
        }
        Located::Absent {
            rel: at,
            mapped: Some(link),
        } => {
            let to = MERGED_USR
                .iter()
                .find(|(l, _)| *l == link)
                .map_or("", |(_, to)| to);
            unknown(format!(
                "{path}: read as /{at}, as /{link} leads to {to} on Arch, and nothing is there"
            ))
        }
        Located::Absent {
            rel: at,
            mapped: None,
        } => mounts.hides(&at).map_or(Program::Other, unknown),
        Located::Unread { last: true, .. }
        | Located::Masked
        | Located::Dir(_)
        | Located::Node(_) => Program::Other,
        Located::Unread { why, .. } | Located::Unknown(why) => unknown(why),
    }
}

/// A regular file a command names, read as `role` runs it: a script if its
/// first bytes are `#!` — or for [`Role::Script`] if it is no binary — read
/// whole unless it is over [`MAX_READ_BYTES`]. A binary is never read past
/// its first four bytes.
fn read_script(root: &Path, rel: &str, role: Role) -> Program {
    let unread = |why: String| match role {
        Role::Argument => Program::Other,
        Role::Program | Role::Script => Program::Unknown(why),
    };
    let mut file = match open_in_root(root, rel) {
        Ok(file) => file,
        Err(ReadErr::Absent) => return Program::Other,
        Err(ReadErr::Unreadable(why)) => return unread(why),
    };
    let mut head = Vec::new();
    if let Err(e) = (&mut file).take(4).read_to_end(&mut head) {
        return unread(format!("{rel}: {e}"));
    }
    let script = match role {
        Role::Script => !head.starts_with(b"\x7fELF"),
        Role::Program | Role::Argument => head.starts_with(b"#!"),
    };
    if !script {
        return Program::Other;
    }
    let mut text = Vec::new();
    if let Err(e) = file
        .seek(SeekFrom::Start(0))
        .and_then(|_| file.take(MAX_READ_BYTES + 1).read_to_end(&mut text))
    {
        return unread(format!("{rel}: {e}"));
    }
    // Bound first: `len as u64 < MAX` would not parse, which hides that
    // comparison from mutation testing.
    let read = text.len() as u64;
    if read > MAX_READ_BYTES {
        return unread(format!("{rel}: larger than 64 KiB, not read"));
    }
    Program::Script(String::from_utf8_lossy(&text).into_owned())
}

/// The units enabled in the trees ([`EnabledUnits`]): every name in every
/// dependency directory, with the directories that name it, highest tree
/// first. A file with a dependency directory's name enables nothing (systemd
/// reads only directories); a symlinked one is refused rather than followed,
/// and as systemd would follow it, the set is then unknown.
fn read_enabled_units(root: &Path, trees: &[Tree]) -> EnabledUnits {
    let mut units: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for tree in trees {
        let names = match &tree.listing {
            Listing::Absent => continue,
            Listing::Names(names) => names,
            Listing::Unreadable(reason) => {
                return EnabledUnits::Unreadable {
                    reason: reason.clone(),
                };
            }
        };
        for entry in names
            .iter()
            .filter(|n| DEPENDENCY_DIRS.iter().any(|suffix| n.ends_with(suffix)))
        {
            let rel = format!("{}/{entry}", tree.rel);
            match entry_at(root, &rel) {
                Entry::Dir => {}
                Entry::File | Entry::Other | Entry::Absent => continue,
                Entry::Symlink(reason) | Entry::Unreadable(reason) => {
                    return EnabledUnits::Unreadable { reason };
                }
            }
            match list_dir(root, &rel) {
                Listing::Names(listed) => {
                    for unit in listed {
                        units.entry(unit).or_default().push(rel.clone());
                    }
                }
                Listing::Absent => {}
                Listing::Unreadable(reason) => return EnabledUnits::Unreadable { reason },
            }
        }
    }
    EnabledUnits::Listed {
        units: units
            .into_iter()
            .map(|(name, dirs)| EnabledUnit { name, dirs })
            .collect(),
    }
}

/// For each unit, the names its own dependency directories hold
/// (`<unit>.wants/` and the others, in every tree).
fn dependency_dirs(units: &[EnabledUnit]) -> BTreeMap<String, Vec<String>> {
    let mut owned: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for unit in units {
        for dir in &unit.dirs {
            let leaf = dir.rsplit('/').next().unwrap_or(dir);
            if let Some(owner) = DEPENDENCY_DIRS
                .iter()
                .find_map(|suffix| leaf.strip_suffix(suffix))
            {
                let names = owned.entry(owner.to_string()).or_default();
                if !names.contains(&unit.name) {
                    names.push(unit.name.clone());
                }
            }
        }
    }
    owned
}

/// The configuration btrbk would read by default ([`BtrbkConfig`]): the first
/// of [`BTRBK_CONFIGS`] that exists, through its links. It is `lstat`ed,
/// never opened: beside something that runs btrbk its presence is the whole
/// signal. One under a mount point is not what that OS sees, and one that is
/// not here may be there once it is up ([`Mounts::hides`]): unknown — but
/// one that surely is there decides first.
fn read_btrbk_config(root: &Path, mounts: &Mounts, probe: MountFlags) -> BtrbkConfig {
    let mut maybe = None;
    for rel in BTRBK_CONFIGS {
        if let Some(reason) = mounts.covers(rel) {
            maybe = maybe.or(Some(reason));
            continue;
        }
        match locate(root, rel, probe) {
            Located::File(at, size_bytes) => match mounts.covers(&at) {
                Some(reason) => maybe = maybe.or(Some(reason)),
                None => {
                    return BtrbkConfig::Present {
                        path: format!("/{rel}"),
                        size_bytes,
                    };
                }
            },
            Located::Absent { rel: at, .. } => maybe = maybe.or_else(|| mounts.hides(&at)),
            Located::Masked => {
                return BtrbkConfig::Unreadable {
                    reason: format!("{rel}: a link to /dev/null, which btrbk would read"),
                };
            }
            Located::Dir(_) | Located::Node(_) => {
                return BtrbkConfig::Unreadable {
                    reason: format!("{rel}: not a regular file"),
                };
            }
            Located::Unread { why, .. } | Located::Unknown(why) => {
                return BtrbkConfig::Unreadable { reason: why };
            }
        }
    }
    maybe.map_or(BtrbkConfig::Absent, |reason| BtrbkConfig::Unreadable {
        reason,
    })
}

/// The template of an instance name: `backup@.service` for
/// `backup@daily.service`; `None` for anything else.
fn template_of(name: &str) -> Option<String> {
    let (stem, kind) = name.rsplit_once('.')?;
    let (prefix, instance) = stem.split_once('@')?;
    (!instance.is_empty()).then(|| format!("{prefix}@.{kind}"))
}

/// The unit type: `service` for `btrbk.service`.
fn unit_type(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(_, kind)| kind)
}

/// `name` without its type: `backup@daily` for `backup@daily.service`.
fn stem_of(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// The unit of the same name and another type: what a timer, path or socket
/// unit starts when it names no other.
fn sibling(name: &str, kind: &str) -> String {
    format!("{}.{kind}", stem_of(name))
}

/// A unit name as `unit`'s settings give it, resolved as systemd resolves it:
/// its specifiers ([`expand_specifiers`]), then a template left over
/// (`snap@.service`) instantiated with `unit`'s instance, or with its prefix
/// when it has none — `Requires=dirmngr@%i.socket` in `ytest.service` is
/// `dirmngr@ytest.socket` (seen with systemd 262's own parser). `None` for a
/// specifier only that OS's systemd can resolve.
fn resolve_name(unit: &str, raw: &str) -> Option<String> {
    let (name, resolved) = expand_specifiers(unit, raw);
    if !resolved {
        return None;
    }
    let (stem, kind) = match name.rsplit_once('.') {
        Some((stem, kind)) if stem.ends_with('@') && stem.matches('@').count() == 1 => (stem, kind),
        _ => return Some(name),
    };
    let own = stem_of(unit);
    let with = match own.split_once('@') {
        Some((_, instance)) if !instance.is_empty() => instance,
        Some((prefix, _)) => prefix,
        None => own,
    };
    Some(format!("{stem}{with}.{kind}"))
}

/// `raw` with the specifiers that come from `unit`'s own name replaced
/// (systemd.unit(5), "Specifiers"): `%i`, `%n`, `%N`, `%p`, `%j` and `%%`.
/// Any other — the host name, a user's home, a runtime path — is left as
/// written, and the flag says so: only that OS's systemd can resolve it. So
/// are the instance and the full name of a template itself (`snap@.service`,
/// started per connection by an `Accept=yes` socket): each connection's
/// instance has its own.
fn expand_specifiers(unit: &str, raw: &str) -> (String, bool) {
    let stem = stem_of(unit);
    let (prefix, instance) = stem.split_once('@').unwrap_or((stem, ""));
    let template = stem.ends_with('@');
    let last = prefix.rsplit_once('-').map_or(prefix, |(_, last)| last);
    let mut out = String::new();
    let mut resolved = true;
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('i') if !template => out.push_str(instance),
            Some('n') if !template => out.push_str(unit),
            Some('N') if !template => out.push_str(stem),
            Some('p') => out.push_str(prefix),
            Some('j') => out.push_str(last),
            Some('%') => out.push('%'),
            other => {
                resolved = false;
                out.push('%');
                out.extend(other);
            }
        }
    }
    (out, resolved)
}

/// A unit's own file.
#[derive(Debug, PartialEq, Eq)]
enum UnitFile {
    Text(String),
    NotFound,
    /// A link to `/dev/null`, or an empty file: masked, it runs nothing.
    Masked,
    /// A link to another unit's file: an alias of that unit.
    Alias(String),
    /// A link in the vendor tree that was not read: a package's alias, and
    /// why it was not read.
    VendorAlias(String),
    Unknown(String),
}

/// Whether `name` is a unit's name: `<something>.<a unit type>`.
fn is_unit_name(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(stem, kind)| !stem.is_empty() && UNIT_TYPES.contains(&kind))
}

/// `name`'s unit file, by systemd's precedence: the first tree that has it;
/// an instance with no file of its own uses its template's. A link is
/// followed ([`locate`]): to `/dev/null` it masks the unit (so does an empty
/// file); to another unit's file it makes `name` that unit's alias, loaded by
/// that unit's own name; to nothing, it is passed over for the next tree. A
/// link that cannot be read is a package's alias in the vendor tree,
/// skipped, and unknown anywhere else — `systemctl link`, `mask` and an
/// `enable` alias then cannot be told apart.
fn find_unit_file(root: &Path, trees: &[Tree], name: &str, probe: MountFlags) -> UnitFile {
    let template = template_of(name);
    for candidate in std::iter::once(name.to_string()).chain(template.clone()) {
        for tree in trees {
            match &tree.listing {
                Listing::Absent => continue,
                Listing::Unreadable(why) => return UnitFile::Unknown(why.clone()),
                Listing::Names(names) if !names.contains(&candidate) => continue,
                Listing::Names(_) => {}
            }
            let rel = format!("{}/{candidate}", tree.rel);
            let at = match locate(root, &rel, probe) {
                Located::Masked => return UnitFile::Masked,
                Located::Unread { why, .. } if tree.rel == VENDOR_TREE => {
                    return UnitFile::VendorAlias(why);
                }
                Located::Unread { why, .. } | Located::Unknown(why) => {
                    return UnitFile::Unknown(why);
                }
                Located::File(at, len) => (at, Some(len)),
                Located::Absent { rel: at, .. } => (at, None),
                Located::Dir(at) | Located::Node(at) => {
                    return UnitFile::Unknown(format!("{at}: not a regular file"));
                }
            };
            let target = at.0.rsplit('/').next().unwrap_or_default();
            // Its own file: of its own name, or an instance's own template.
            let own = target == candidate || template.as_deref() == Some(target);
            if !own && is_unit_name(target) {
                // A link to a template aliases the same instance of that one —
                // a template's link every instance, an instance's link just
                // that one (systemd.unit(5), "Aliases").
                let alias = match target.split_once("@.") {
                    Some((prefix, kind)) => {
                        let instance = stem_of(name).split_once('@').map_or("", |(_, i)| i);
                        format!("{prefix}@{instance}.{kind}")
                    }
                    None => target.to_string(),
                };
                return UnitFile::Alias(alias);
            }
            match at.1 {
                // A link to nothing, or one gone since the listing.
                None => continue,
                Some(0) => return UnitFile::Masked,
                Some(len) if len > MAX_READ_BYTES => {
                    return UnitFile::Unknown(format!("{rel}: larger than 64 KiB, not read"));
                }
                Some(_) => {
                    return match read_in_root(root, &at.0) {
                        Ok(text) => UnitFile::Text(text),
                        Err(ReadErr::Absent) => {
                            UnitFile::Unknown(format!("{rel}: listed, then gone"))
                        }
                        Err(ReadErr::Unreadable(why)) => UnitFile::Unknown(why),
                    };
                }
            }
        }
    }
    UnitFile::NotFound
}

/// The drop-in directories of `name`, most specific first (systemd.unit(5)):
/// its own, its template's, those of its dash-truncated prefixes
/// (`foo-bar-.service.d`, `foo-.service.d`), and its type's (`service.d`).
fn drop_in_dirs(name: &str) -> Vec<String> {
    let mut dirs = vec![format!("{name}.d")];
    dirs.extend(template_of(name).map(|t| format!("{t}.d")));
    if let Some((stem, kind)) = name.rsplit_once('.') {
        let mut dashes: Vec<usize> = stem.match_indices('-').map(|(i, _)| i).collect();
        dashes.reverse();
        dirs.extend(
            dashes
                .into_iter()
                .map(|i| format!("{}.{kind}.d", &stem[..=i])),
        );
        dirs.push(format!("{kind}.d"));
    }
    dirs
}

/// The texts of the drop-ins of `names` (a unit and its aliases) in the
/// order systemd applies them: each file name once — the most specific
/// directory, then the highest tree, wins a name, so the first met in that
/// order — sorted by file name across every directory. Links are followed
/// ([`locate`]): one to `/dev/null` masks that drop-in. One that cannot be
/// read is a package's in the vendor tree, skipped, and unknown elsewhere.
fn read_drop_ins(
    root: &Path,
    trees: &[Tree],
    names: &[String],
    probe: MountFlags,
) -> Result<Vec<String>, String> {
    let mut chosen: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let dirs: Vec<String> = names.iter().flat_map(|n| drop_in_dirs(n)).collect();
    for dir in dirs {
        for (rank, tree) in trees.iter().enumerate() {
            // A name its listing lacks is absent to `locate` too.
            match &tree.listing {
                Listing::Absent => continue,
                Listing::Unreadable(why) => return Err(why.clone()),
                Listing::Names(_) => {}
            }
            let rel = format!("{}/{dir}", tree.rel);
            let at = match locate(root, &rel, probe) {
                Located::Dir(at) => at,
                Located::Unread { .. } if tree.rel == VENDOR_TREE => continue,
                Located::Unread { why, .. } | Located::Unknown(why) => return Err(why),
                // A file with a drop-in directory's name holds no drop-ins;
                // nor does a link to nothing or to /dev/null.
                Located::File(..) | Located::Node(_) | Located::Absent { .. } | Located::Masked => {
                    continue;
                }
            };
            let files = match list_dir(root, &at) {
                Listing::Names(files) => files,
                Listing::Absent => return Err(format!("{rel}: listed, then gone")),
                Listing::Unreadable(why) => return Err(why),
            };
            for file in files.into_iter().filter(|f| f.ends_with(".conf")) {
                let path = format!("{at}/{file}");
                chosen.entry(file).or_insert((rank, path));
            }
        }
    }
    let mut texts = Vec::new();
    for (_, (rank, rel)) in chosen {
        match read_file(root, &rel, probe) {
            FileRead::Text(text) => texts.push(text),
            FileRead::Nothing | FileRead::NotFile => {}
            FileRead::Unread(_) if trees[rank].rel == VENDOR_TREE => {}
            FileRead::Unread(why) | FileRead::Unknown(why) => return Err(why),
        }
    }
    Ok(texts)
}

/// What a unit file and its drop-ins set that matters here.
#[derive(Debug, Default, PartialEq)]
struct UnitSettings {
    /// Each `Exec…=` key of `[Service]` or `[Socket]` and its commands, an
    /// empty assignment clearing the list as systemd does.
    exec: BTreeMap<String, Vec<String>>,
    /// Each [`PULLS`] setting of `[Unit]` and one unit it names, in order.
    pulls: Vec<(String, String)>,
    /// `[Timer]`/`[Path]` `Unit=`.
    unit: Option<String>,
    /// `[Socket]` `Service=`: the last that names a service.
    service: Option<String>,
    /// `[Socket]` `Accept=`.
    accept: bool,
    /// `[Mount]` `Type=`.
    mount_type: Option<String>,
    persistent: bool,
    on_calendar: bool,
    /// `OnBootSec=`, `OnStartupSec=` or `OnActiveSec=`: due soon after boot.
    on_boot: bool,
}

/// systemd's boolean (`parse_boolean`): `None` for anything else, an empty
/// value included, which systemd then ignores ("Failed to parse …,
/// ignoring") and leaves the setting as it was.
fn parse_boolean(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "yes" | "y" | "true" | "t" | "on" => Some(true),
        "0" | "no" | "n" | "false" | "f" | "off" => Some(false),
        _ => None,
    }
}

/// A unit file's lines as systemd reads them (its `config_parse`): a comment
/// line is dropped whole, in or out of a continuation — it is skipped before
/// any trailing backslash is looked at, so it never continues — and a line
/// ending in an odd run of backslashes is joined to the next, its last
/// backslash replaced by a space (an even run is escaped backslashes).
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current: Option<String> = None;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let trailing = raw.chars().rev().take_while(|&c| c == '\\').count();
        let line = current.get_or_insert_with(String::new);
        if trailing % 2 == 1 {
            line.push_str(&raw[..raw.len() - 1]);
            line.push(' ');
        } else {
            line.push_str(raw);
            lines.extend(current.take());
        }
    }
    lines.extend(current);
    lines
}

/// The settings of a unit file followed by its drop-ins, in order. Each text
/// starts outside any section, as a drop-in must name its own.
fn parse_unit(texts: impl IntoIterator<Item = String>) -> UnitSettings {
    let mut settings = UnitSettings::default();
    for text in texts {
        let mut section = String::new();
        for line in logical_lines(&text) {
            // Comments are gone already; a blank line has no `=` and no `[`.
            let line = line.trim();
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.to_string();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match (section.as_str(), key) {
                ("Service" | "Socket", _) if key.starts_with("Exec") => {
                    if value.is_empty() {
                        settings.exec.remove(key);
                    } else {
                        settings
                            .exec
                            .entry(key.to_string())
                            .or_default()
                            .push(value.to_string());
                    }
                }
                ("Unit", _) if PULLS.contains(&key) => settings.pulls.extend(
                    value
                        .split_whitespace()
                        .map(|name| (key.to_string(), name.to_string())),
                ),
                ("Timer" | "Path", "Unit") => {
                    settings.unit = (!value.is_empty()).then(|| value.to_string());
                }
                ("Socket", "Service") if value.ends_with(".service") => {
                    settings.service = Some(value.to_string());
                }
                ("Socket", "Accept") => {
                    settings.accept = parse_boolean(value).unwrap_or(settings.accept);
                }
                ("Mount", "Type") => {
                    settings.mount_type = (!value.is_empty()).then(|| value.to_string());
                }
                ("Timer", "Persistent") => {
                    settings.persistent = parse_boolean(value).unwrap_or(settings.persistent);
                }
                // An empty one resets every timer, calendar and monotonic
                // alike (systemd.timer(5)).
                (
                    "Timer",
                    "OnActiveSec" | "OnBootSec" | "OnStartupSec" | "OnUnitActiveSec"
                    | "OnUnitInactiveSec" | "OnCalendar",
                ) if value.is_empty() => {
                    settings.on_boot = false;
                    settings.on_calendar = false;
                }
                ("Timer", "OnActiveSec" | "OnBootSec" | "OnStartupSec") => settings.on_boot = true,
                ("Timer", "OnCalendar") => settings.on_calendar = true,
                _ => {}
            }
        }
    }
    settings
}

/// A unit read with its drop-ins.
enum Loaded {
    Settings(UnitSettings),
    NotFound,
    Masked,
    Alias(String),
    VendorAlias(String),
    Unknown(String),
}

/// `name`'s file ([`find_unit_file`]) with the drop-ins of `name` and of the
/// aliases it was reached through.
fn load_unit(
    root: &Path,
    trees: &[Tree],
    name: &str,
    aliases: &[String],
    probe: MountFlags,
) -> Loaded {
    let main = match find_unit_file(root, trees, name, probe) {
        UnitFile::Text(text) => text,
        UnitFile::NotFound => return Loaded::NotFound,
        UnitFile::Masked => return Loaded::Masked,
        UnitFile::Alias(target) => return Loaded::Alias(target),
        UnitFile::VendorAlias(why) => return Loaded::VendorAlias(why),
        UnitFile::Unknown(why) => return Loaded::Unknown(why),
    };
    let names: Vec<String> = std::iter::once(name.to_string())
        .chain(aliases.iter().cloned())
        .collect();
    match read_drop_ins(root, trees, &names, probe) {
        Ok(drop_ins) => Loaded::Settings(parse_unit(std::iter::once(main).chain(drop_ins))),
        Err(why) => Loaded::Unknown(why),
    }
}

/// The words of a command or a script line: split at blanks and at shell
/// operators, each operator a word of its own; quotes group and are removed,
/// a backslash escapes the next character. systemd's `|` prefix is split off
/// as an operator, before the word it prefixes.
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) if c == '\\' => word.extend(chars.next()),
            Some(_) => word.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    word.extend(chars.next());
                    in_word = true;
                }
                c if c.is_whitespace() => {
                    if in_word {
                        out.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                ';' | '&' | '|' | '(' | ')' | '`' => {
                    if in_word {
                        out.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                    out.push(c.to_string());
                }
                c => {
                    word.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        out.push(word);
    }
    out
}

/// `words`, with a quoted script that names btrbk — `sh -c 'btrbk run'` —
/// split into its own words in place.
fn command_words(words: &[String]) -> Vec<String> {
    words
        .iter()
        .flat_map(|word| {
            let inner = self::words(word);
            if inner.iter().any(|w| naming(w) != Naming::Not) {
                inner
            } else {
                vec![word.clone()]
            }
        })
        .collect()
}

/// How a word names btrbk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Naming {
    /// `btrbk`, or a path ending in it.
    Btrbk,
    /// A last component that only contains it: `btrbk.sh`, `run-btrbk`,
    /// `/etc/btrbk.conf`. What that runs is not known.
    Like,
    Not,
}

/// How `word` names btrbk, after systemd's command prefixes (`-`, `@`, `:`,
/// `+`, `!`). Any word counts, the cautious reading: `/etc/btrbk` passed as
/// an argument is btrbk's name too, and `systemctl start btrbk.service`
/// starts it.
fn naming(word: &str) -> Naming {
    let word = word.trim_start_matches(['@', '-', ':', '+', '!']);
    let last = word.rsplit_once('/').map_or(word, |(_, last)| last);
    if last == BTRBK {
        Naming::Btrbk
    } else if last.contains(BTRBK) {
        Naming::Like
    } else {
        Naming::Not
    }
}

/// The config a btrbk command line gives.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigArg {
    /// None: btrbk looks for its default config.
    Default,
    /// An absolute literal path.
    Path(String),
    /// Something only that OS could resolve (a variable, a specifier, a
    /// relative path, a command substitution among its arguments), or a
    /// short-option cluster this does not unpick.
    Unresolved(String),
}

fn config_arg(raw: &str) -> ConfigArg {
    if raw.starts_with('/') && !raw.contains(['$', '%', '*', '?', '[', '~', '`']) {
        ConfigArg::Path(raw.to_string())
    } else {
        ConfigArg::Unresolved(raw.to_string())
    }
}

/// What a command or script line runs that is btrbk's.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Found {
    /// btrbk, with the config it is given.
    Btrbk(ConfigArg),
    /// A word [`Naming::Like`] btrbk's.
    Like(String),
}

/// Whether a word ends the arguments of the command before it: an operator,
/// or a comment.
fn ends_command(word: &str) -> bool {
    OPERATORS.contains(&word) || word.starts_with('#')
}

/// The config btrbk's arguments `args` give: the `-c FILE`, `-cFILE`,
/// `--config FILE` or `--config=FILE` among them. Any argument with a
/// variable or a command substitution in it — `"$@"` forwarded by a
/// wrapper, `$OPTS` from the unit's environment — could carry a `-c` only
/// that OS would see, so the config is unresolved; so is one a backtick
/// supplies. The arguments end at an operator or a comment.
fn btrbk_config(args: &[String]) -> ConfigArg {
    let mut arg = ConfigArg::Default;
    let mut unresolved = None;
    let mut rest = args.iter().peekable();
    while let Some(w) = rest.next() {
        if w == "`" {
            unresolved = unresolved.or_else(|| Some(w.clone()));
            break;
        }
        if ends_command(w) {
            break;
        }
        if w == "-c" || w == "--config" {
            arg = rest
                .next_if(|p| !ends_command(p))
                .map_or_else(|| ConfigArg::Unresolved(w.clone()), |p| config_arg(p));
        } else if let Some(path) = w.strip_prefix("--config=") {
            arg = config_arg(path);
        } else if let Some(path) = w.strip_prefix("-c") {
            arg = config_arg(path);
        } else if w.starts_with('-')
            && w.contains('c')
            && w[1..].chars().all(|c| c.is_ascii_alphabetic())
        {
            arg = ConfigArg::Unresolved(w.clone());
        } else if w.contains('$') {
            unresolved = unresolved.or_else(|| Some(w.clone()));
        }
    }
    match (unresolved, &arg) {
        (Some(word), ConfigArg::Default | ConfigArg::Path(_)) => ConfigArg::Unresolved(word),
        _ => arg,
    }
}

/// What a command or script line runs that is btrbk's: each btrbk
/// invocation with the config its arguments give ([`btrbk_config`]) — its
/// arguments are not looked at again — and each word only like btrbk's
/// name.
fn btrbk_invocations(line: &[String]) -> Vec<Found> {
    let words = command_words(line);
    let mut found = Vec::new();
    let mut rest = &words[..];
    // Within a backtick substitution: the next backtick closes it.
    let mut inside = false;
    while let Some((word, tail)) = rest.split_first() {
        rest = tail;
        if word == "`" {
            inside = !inside;
            continue;
        }
        match naming(word) {
            Naming::Btrbk => {}
            Naming::Like => {
                found.push(Found::Like(word.clone()));
                continue;
            }
            Naming::Not => continue,
        }
        let end = rest
            .iter()
            .position(|w| ends_command(w))
            .unwrap_or(rest.len());
        // A backtick right after that opens a substitution: what that
        // substitutes is btrbk's argument. One that closes the substitution
        // btrbk runs in is not.
        let args_end = if !inside && rest.get(end).is_some_and(|w| w == "`") {
            end + 1
        } else {
            end
        };
        found.push(Found::Btrbk(btrbk_config(&rest[..args_end])));
        rest = &rest[end..];
    }
    found
}

/// The command lines of a shell script as sh reads them: a backslash at the
/// end of a line joins the next to it, a quoted string may run across
/// lines, a `#` that starts a word starts a comment to the end of the line,
/// and the lines of a here-document are data, not commands. `Err` when the
/// text ends inside a quote or after a joining backslash: where the command
/// ends cannot be told.
fn shell_lines(text: &str) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut heredocs: VecDeque<(String, bool)> = VecDeque::new();
    for raw in text.lines() {
        if let Some((delimiter, tabs)) = heredocs.front() {
            let line = if *tabs {
                raw.trim_start_matches('\t')
            } else {
                raw
            };
            if line == delimiter {
                heredocs.pop_front();
            }
            continue;
        }
        let mut chars = raw.chars().peekable();
        let mut word_start = current.is_empty() || current.ends_with(char::is_whitespace);
        let mut joined = false;
        while let Some(c) = chars.next() {
            match quote {
                Some('\'') if c == '\'' => quote = None,
                Some('"') if c == '"' => quote = None,
                Some('"') if c == '\\' => {
                    current.push(c);
                    current.extend(chars.next());
                    continue;
                }
                Some(_) => {}
                None => match c {
                    '\'' | '"' => quote = Some(c),
                    '\\' if chars.peek().is_none() => {
                        joined = true;
                        continue;
                    }
                    '\\' => {
                        current.push(c);
                        current.extend(chars.next());
                        word_start = false;
                        continue;
                    }
                    '#' if word_start => break,
                    _ => {}
                },
            }
            current.push(c);
            word_start = quote.is_none()
                && (c.is_whitespace() || OPERATORS.contains(&c.to_string().as_str()));
        }
        if joined {
            continue;
        }
        if quote.is_some() {
            current.push(' ');
            continue;
        }
        let line = std::mem::take(&mut current);
        heredocs.extend(heredoc_delimiters(&line));
        lines.push(line);
    }
    if quote.is_some() {
        return Err("it ends inside a quote".to_string());
    }
    if !current.is_empty() {
        return Err("its last line ends in a backslash".to_string());
    }
    Ok(lines)
}

/// The here-documents a command line opens (`<<EOF`, `<< 'EOF'`, `<<-EOF`):
/// each delimiter, and whether its lines may be indented with tabs.
fn heredoc_delimiters(line: &str) -> Vec<(String, bool)> {
    let words = words(line);
    let mut found = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let Some(rest) = word.strip_prefix("<<") else {
            continue;
        };
        if rest.starts_with('<') {
            continue; // a here-string, `<<<`
        }
        let (tabs, rest) = match rest.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        let delimiter = if rest.is_empty() {
            words.get(i + 1).cloned().unwrap_or_default()
        } else {
            rest.to_string()
        };
        if !delimiter.is_empty() {
            found.push((delimiter, tabs));
        }
    }
    found
}

/// How a line is read: as systemd reads an `Exec…=` command, or as sh reads
/// a command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grammar {
    /// systemd's: commands split at a lone `;`, the first word of each the
    /// program, taken literally ("may not be a variable").
    Systemd,
    /// sh's: commands split at every operator, after any `NAME=value` and
    /// keyword, a variable in a word expanded at run time.
    Shell,
}

/// What one command line runs: a program, or a script a shell or an
/// interpreter is given (with the words after it), or a command line passed
/// on whole (`sh -c`, `flock -c`, `env -S`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Launch {
    Program {
        word: String,
        role: Role,
        /// systemd's own program word, taken literally.
        literal: bool,
        /// The words after it.
        args: Vec<String>,
    },
    Inline(String),
}

/// Whether `word` is a shell assignment, `NAME=value`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// What one command line runs ([`Launch`]): the program of each command in
/// it, and through each wrapper, shell and interpreter what that runs.
fn launches(words: &[String], grammar: Grammar) -> Vec<Launch> {
    let mut found = Vec::new();
    let separates = |w: &String| match grammar {
        Grammar::Systemd => w == ";",
        Grammar::Shell => OPERATORS.contains(&w.as_str()),
    };
    for words in words.split(separates) {
        command(words, grammar, &mut found);
    }
    found
}

/// The program of one command, after systemd's prefixes (`@` gives it an
/// `argv[0]`, the next word) or after sh's assignments and keywords.
fn command(words: &[String], grammar: Grammar, found: &mut Vec<Launch>) {
    let Some(first) = words.first() else {
        return;
    };
    match grammar {
        Grammar::Systemd => {
            let program = first.trim_start_matches(['@', '-', ':', '+', '!']);
            let prefixes = first.strip_suffix(program).unwrap_or_default();
            let mut words = words.to_vec();
            words[0] = program.to_string();
            if prefixes.contains('@') && words.len() > 1 {
                words.remove(1);
            }
            run_from(&words, 0, Role::Program, true, found);
        }
        Grammar::Shell => {
            for (i, word) in words.iter().enumerate() {
                if HEADERS.contains(&word.as_str()) {
                    return;
                }
                if !KEYWORDS.contains(&word.as_str()) && !is_assignment(word) {
                    run_from(words, i, Role::Program, false, found);
                    return;
                }
            }
        }
    }
}

/// The program at `words[i]`, and what it runs in turn: a wrapper's command,
/// a shell's script or `-c` line, an interpreter's script.
fn run_from(words: &[String], i: usize, role: Role, literal: bool, found: &mut Vec<Launch>) {
    let word = &words[i];
    found.push(Launch::Program {
        word: word.clone(),
        role,
        literal,
        args: words[i + 1..].to_vec(),
    });
    if role != Role::Program {
        return;
    }
    let name = word.rsplit('/').next().unwrap_or(word);
    if SHELLS.contains(&name) {
        shell_operand(words, i + 1, found);
    } else if INTERPRETERS.contains(&name) {
        interpreter_operand(words, i + 1, found);
    } else if let Some(wrapper) = WRAPPERS.iter().find(|w| w.name == name) {
        wrapper_operand(wrapper, words, i + 1, found);
    }
}

/// A shell's `-c` command line (`-c`, or a cluster with `c` such as `-ec`),
/// else the script it runs: its first operand.
fn shell_operand(words: &[String], from: usize, found: &mut Vec<Launch>) {
    let mut rest = words.iter().enumerate().skip(from);
    while let Some((j, w)) = rest.next() {
        let cluster = w.starts_with('-')
            && w[1..].chars().all(|c| c.is_ascii_alphabetic())
            && w.contains('c');
        if cluster {
            found.extend(words.get(j + 1).map(|line| Launch::Inline(line.clone())));
            return;
        }
        if matches!(
            w.as_str(),
            "-o" | "+o" | "-O" | "+O" | "--rcfile" | "--init-file"
        ) {
            rest.next(); // its value
        } else if w.starts_with('-') || w.starts_with('+') {
            // An option.
        } else {
            run_from(words, j, Role::Script, false, found);
            return;
        }
    }
}

/// An interpreter's script: its first operand, unless it runs code or a
/// module given on the line instead.
fn interpreter_operand(words: &[String], from: usize, found: &mut Vec<Launch>) {
    let mut rest = words.iter().enumerate().skip(from);
    while let Some((j, w)) = rest.next() {
        if INTERPRETER_CODE.contains(&w.as_str()) {
            return;
        }
        if INTERPRETER_VALUED.contains(&w.as_str()) {
            rest.next(); // its value
        } else if w.starts_with('-') {
            // An option.
        } else {
            run_from(words, j, Role::Script, false, found);
            return;
        }
    }
}

/// The command a wrapper runs: past its options, their values, its own
/// operands and (for `env`) assignments; or the command line one of its
/// options passes.
fn wrapper_operand(wrapper: &Wrapper, words: &[String], from: usize, found: &mut Vec<Launch>) {
    let mut operands = 0..wrapper.operands;
    let mut options = true;
    let mut rest = words.iter().enumerate().skip(from);
    while let Some((j, w)) = rest.next() {
        if options && wrapper.command.contains(&w.as_str()) {
            found.extend(words.get(j + 1).map(|line| Launch::Inline(line.clone())));
            return;
        }
        if let Some((flag, line)) = w.split_once('=')
            && options
            && wrapper.command.contains(&flag)
        {
            found.push(Launch::Inline(line.to_string()));
            return;
        }
        if options && w == "--" {
            options = false;
        } else if options && w.starts_with('-') {
            // `-` alone too: env's -i, su's login shell.
            if wrapper.valued.contains(&w.as_str()) {
                rest.next(); // its value
            }
        } else if wrapper.name == "env" && is_assignment(w) {
            // An assignment env makes.
        } else if operands.next().is_none() {
            run_from(words, j, Role::Program, false, found);
            return;
        }
    }
}

/// When a unit runs once that OS is up, most urgent first ([`When::rank`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum When {
    EveryBoot,
    StraightAfterBoot,
    SoonAfterBoot,
    CatchUpOrNext,
    NextScheduled,
    Connects,
    PathMet,
    Fails(String),
    Succeeds(String),
    /// When a target a system reaches only to stop, sleep or start from its
    /// initrd starts.
    Starts(String),
}

impl When {
    /// How soon: a unit reached again sooner than before passes that on.
    fn rank(&self) -> u8 {
        match self {
            When::EveryBoot => 0,
            When::StraightAfterBoot | When::SoonAfterBoot => 1,
            When::CatchUpOrNext => 2,
            When::NextScheduled => 3,
            When::Connects | When::PathMet | When::Fails(_) | When::Succeeds(_) => 4,
            When::Starts(_) => 5,
        }
    }

    fn text(&self) -> String {
        match self {
            When::EveryBoot => "at every boot".into(),
            When::StraightAfterBoot => "straight after boot (Persistent catch-up)".into(),
            When::SoonAfterBoot => {
                "soon after boot (OnBootSec=, OnStartupSec= or OnActiveSec=)".into()
            }
            When::CatchUpOrNext => {
                "straight after boot or at its next scheduled time (its stamp is unknown)".into()
            }
            When::NextScheduled => "at its next scheduled time after boot".into(),
            When::Connects => "when something connects to it".into(),
            When::PathMet => "when its path condition is met".into(),
            When::Fails(unit) => format!("if {unit} fails"),
            When::Succeeds(unit) => format!("after {unit} succeeds"),
            When::Starts(target) => format!("when {target} starts"),
        }
    }
}

/// When a unit enabled by `dirs` runs: at every boot, unless every target
/// that pulls it in is one a system reaches only to stop, sleep or start from
/// its initrd.
fn boot_when(dirs: &[String]) -> When {
    let wanters: Vec<&str> = dirs
        .iter()
        .filter_map(|dir| {
            let leaf = dir.rsplit('/').next()?;
            DEPENDENCY_DIRS
                .iter()
                .find_map(|suffix| leaf.strip_suffix(suffix))
        })
        .collect();
    let at_boot = wanters
        .iter()
        .any(|w| !NOT_AT_BOOT.iter().any(|prefix| w.starts_with(prefix)));
    match wanters.first() {
        Some(first) if !at_boot => When::Starts((*first).to_string()),
        _ => When::EveryBoot,
    }
}

/// When a timer's service runs. systemd.timer(5): with `Persistent=true` and
/// `OnCalendar=`, the last trigger is stored on disk
/// (`/var/lib/systemd/timers/stamp-<timer>`) and a timer activated at boot
/// triggers at once if it would have triggered while the system was off. The
/// man page does not say what happens with no stamp yet; this reads it as
/// nothing to catch up from, so the next calendar time — an inference, not a
/// documented guarantee.
fn timer_when(root: &Path, timer: &str, settings: &UnitSettings) -> When {
    if settings.on_boot {
        return When::SoonAfterBoot;
    }
    if settings.persistent && settings.on_calendar {
        return match entry_at(root, &format!("var/lib/systemd/timers/stamp-{timer}")) {
            Entry::File => When::StraightAfterBoot,
            Entry::Absent => When::NextScheduled,
            _ => When::CatchUpOrNext,
        };
    }
    When::NextScheduled
}

/// A unit the walk reaches: what starts it, when it runs, whether a name on
/// the way is btrbk's, and the aliases it was reached by.
#[derive(Debug, Clone)]
struct Pull {
    name: String,
    via: Option<String>,
    when: When,
    named: bool,
    aliases: Vec<String>,
}

impl Pull {
    /// How a reason names it: `btrbk.service`, or `btrbk.timer starts
    /// btrbk.service, which`.
    fn who(&self) -> String {
        match &self.via {
            Some(via) => format!("{via} starts {}, which", self.name),
            None => self.name.clone(),
        }
    }
}

/// How a unit a unit starts gets its [`When`].
#[derive(Debug, Clone)]
enum How {
    /// The starting unit's own.
    Inherit,
    /// Its own: what a timer, path or socket unit starts.
    Fixed(When),
    Fails,
    Succeeds,
}

/// A unit a unit starts.
#[derive(Debug, Clone)]
struct Start {
    name: String,
    how: How,
    named: bool,
}

/// What one unit was found to do, on its first reach.
#[derive(Debug, Clone, Default)]
struct UnitRead {
    runs: Runs,
    starts: Vec<Start>,
    /// The unit it is an alias of.
    alias: Option<String>,
    /// The most urgent [`When::rank`] it has been reached with.
    best: u8,
}

/// What a unit's commands run that is btrbk: each invocation's config, and
/// the script it is in when its command does not run btrbk itself.
type Runs = Vec<(Option<String>, ConfigArg)>;

/// What the config a runner would use is.
enum Outcome {
    Present(String),
    Absent(String),
    Unknown(String),
}

impl Outcome {
    fn present(&self) -> Option<bool> {
        match self {
            Outcome::Present(_) => Some(true),
            Outcome::Absent(_) => Some(false),
            Outcome::Unknown(_) => None,
        }
    }
}

/// The default config, as a runner without `-c` would find it.
fn default_outcome(config: &BtrbkConfig) -> Outcome {
    match config {
        BtrbkConfig::Present { path, .. } => Outcome::Present(path.clone()),
        BtrbkConfig::Absent => Outcome::Absent(format!(
            "/{} and /{} are absent",
            BTRBK_CONFIGS[0], BTRBK_CONFIGS[1]
        )),
        BtrbkConfig::Unreadable { reason } => Outcome::Unknown(reason.clone()),
    }
}

/// A config named by `-c`, checked under the root like the default one.
fn path_outcome(root: &Path, mounts: &Mounts, probe: MountFlags, path: &str) -> Outcome {
    let rel = path.trim_start_matches('/');
    if let Some(why) = mounts.covers(rel) {
        return Outcome::Unknown(why);
    }
    match locate(root, rel, probe) {
        Located::File(at, _) => match mounts.covers(&at) {
            Some(why) => Outcome::Unknown(why),
            None => Outcome::Present(path.to_string()),
        },
        Located::Absent { rel: at, .. } => mounts.hides(&at).map_or_else(
            || Outcome::Absent(format!("{path} is absent")),
            Outcome::Unknown,
        ),
        Located::Masked => Outcome::Unknown(format!(
            "{rel}: a link to /dev/null, which btrbk would read"
        )),
        Located::Dir(_) | Located::Node(_) => {
            Outcome::Unknown(format!("{rel}: not a regular file"))
        }
        Located::Unread { why, .. } | Located::Unknown(why) => Outcome::Unknown(why),
    }
}

/// Where a command line is read: how a reason names what runs it, how many
/// scripts deep it is, and the script it is in.
#[derive(Debug, Clone, Copy)]
struct At<'s> {
    who: &'s str,
    depth: usize,
    script: Option<&'s str>,
}

/// Everything found about btrbk in one OS.
struct Findings<'a> {
    root: &'a Path,
    trees: &'a [Tree],
    mounts: &'a Mounts,
    probe: MountFlags,
    default_config: &'a BtrbkConfig,
    /// Each unit's own dependency directories' names ([`dependency_dirs`]).
    owned: BTreeMap<String, Vec<String>>,
    units: BTreeMap<String, UnitRead>,
    /// Each script read, and what it runs.
    scripts: BTreeMap<String, Runs>,
    capped: bool,
    /// What is left of [`MAX_PULLS`].
    pulls: std::ops::Range<usize>,
    runners: Vec<(BtrbkRunner, Outcome)>,
    unknowns: Vec<String>,
    problems: Vec<String>,
}

impl Findings<'_> {
    /// Something that could not be told, and the read behind it, if any;
    /// each once.
    fn unknown(&mut self, reason: String, problem: Option<String>) {
        if !self.unknowns.contains(&reason) {
            self.unknowns.push(reason);
        }
        self.problems.extend(problem);
    }

    /// The walk: first what `default.target` starts, at every boot; then
    /// what the enabled names start that the boot did not reach.
    fn walk(&mut self, enabled: &[EnabledUnit]) {
        let mut queue = VecDeque::from([Pull {
            name: DEFAULT_TARGET.to_string(),
            via: None,
            when: When::EveryBoot,
            named: false,
            aliases: Vec::new(),
        }]);
        self.run(&mut queue);
        queue.extend(enabled.iter().map(|u| Pull {
            name: u.name.clone(),
            via: None,
            when: boot_when(&u.dirs),
            named: u.name.contains(BTRBK),
            aliases: Vec::new(),
        }));
        self.run(&mut queue);
        if let Some(daemon) = CRON_UNITS
            .iter()
            .find(|unit| self.units.contains_key(**unit))
        {
            self.cron(daemon);
        }
    }

    /// Each pull in turn: a unit not read yet is read ([`Findings::unit`]);
    /// one read before, reached sooner than ever, passes that on to what it
    /// starts. Every reach reports what the unit runs.
    fn run(&mut self, queue: &mut VecDeque<Pull>) {
        while let Some(pull) = queue.pop_front() {
            if self.capped {
                return;
            }
            if self.pulls.next().is_none() {
                self.unknowns.push(format!(
                    "more than {} starts at boot: the rest were not followed",
                    self.pulls.end
                ));
                self.capped = true;
                return;
            }
            let rank = pull.when.rank();
            let next = match self.units.get_mut(&pull.name) {
                None => {
                    if self.units.len() == MAX_UNITS {
                        self.unknowns.push(format!(
                            "more than {MAX_UNITS} units start at boot: the rest were not read"
                        ));
                        self.capped = true;
                        return;
                    }
                    let read = self.unit(&pull);
                    let next = starts_of(&read, &pull, false);
                    self.units.insert(pull.name.clone(), read);
                    next
                }
                Some(read) if rank < read.best => {
                    read.best = rank;
                    starts_of(read, &pull, true)
                }
                Some(_) => Vec::new(),
            };
            queue.extend(next);
            for (script, arg) in self.units[&pull.name].runs.clone() {
                self.runner(
                    &pull.name,
                    pull.via.as_deref(),
                    script,
                    &pull.when.text(),
                    arg,
                );
            }
        }
    }

    /// One unit, on its first reach: what its commands run, and the units it
    /// starts — those its `[Unit]` section pulls in, those its dependency
    /// directories name, what a timer, path or socket unit starts.
    fn unit(&mut self, pull: &Pull) -> UnitRead {
        let name = pull.name.as_str();
        let who = pull.who();
        let mut read = UnitRead {
            best: pull.when.rank(),
            ..UnitRead::default()
        };
        let settings = match load_unit(self.root, self.trees, name, &pull.aliases, self.probe) {
            Loaded::Settings(settings) => settings,
            Loaded::Alias(target) => {
                read.alias = Some(target);
                return read;
            }
            Loaded::Masked => return read,
            Loaded::NotFound => {
                self.named_only(&who, pull.named, "was not found");
                return read;
            }
            // What the boot starts must be known: its link unread, it may.
            Loaded::VendorAlias(why) if name == DEFAULT_TARGET => {
                self.unknown(format!("{who} may run btrbk: {why}"), Some(why));
                return read;
            }
            Loaded::VendorAlias(_) => {
                self.named_only(&who, pull.named, "is a package's alias, not followed");
                return read;
            }
            Loaded::Unknown(why) => {
                self.unknown(format!("{who} may run btrbk: {why}"), Some(why));
                return read;
            }
        };
        let kind = unit_type(name);
        if pull.named && !STARTERS.contains(&kind) {
            self.unknown(format!("{who} is named for btrbk"), None);
        }
        for command in settings.exec.values().flatten() {
            let runs = self.exec_line(&who, name, command);
            read.runs.extend(runs);
        }
        let activated = match kind {
            "timer" => Some((
                settings.unit.clone(),
                timer_when(self.root, name, &settings),
            )),
            "path" => Some((settings.unit.clone(), When::PathMet)),
            "socket" if settings.accept => Some((
                // A service instance per connection: only its template is known.
                Some(format!("{}@.service", stem_of(name))),
                When::Connects,
            )),
            "socket" => Some((settings.service.clone(), When::Connects)),
            _ => None,
        };
        if let Some((target, when)) = activated {
            let target = match target {
                Some(target) if !settings.accept => resolve_name(name, &target),
                Some(target) => Some(target),
                None => Some(sibling(name, "service")),
            };
            match target {
                Some(target) => read.starts.push(Start {
                    named: pull.named || target.contains(BTRBK),
                    name: target,
                    how: How::Fixed(when),
                }),
                None => self.unknown(
                    format!("{who} starts a unit only that OS's systemd can name"),
                    None,
                ),
            }
        }
        for (key, raw) in &settings.pulls {
            let Some(target) = resolve_name(name, raw) else {
                self.unknown(
                    format!("{who} pulls in {raw}, which only that OS's systemd can resolve"),
                    None,
                );
                continue;
            };
            let how = match key.as_str() {
                "OnFailure" => How::Fails,
                "OnSuccess" => How::Succeeds,
                _ => How::Inherit,
            };
            read.starts.push(Start {
                named: target.contains(BTRBK),
                name: target,
                how,
            });
        }
        let owners = std::iter::once(name.to_string()).chain(template_of(name));
        let owned: Vec<String> = owners
            .filter_map(|owner| self.owned.get(&owner).cloned())
            .flatten()
            .collect();
        for target in owned {
            read.starts.push(Start {
                named: target.contains(BTRBK),
                name: target,
                how: How::Inherit,
            });
        }
        read
    }

    /// A unit whose file says nothing: unknown only when a name on the way is
    /// btrbk's.
    fn named_only(&mut self, who: &str, named: bool, what: &str) {
        if named {
            self.unknown(
                format!("{who} is named for btrbk, and its unit file {what}"),
                None,
            );
        }
    }

    /// One `Exec…=` command of `unit`, read as systemd reads it: its own
    /// specifiers resolved; a program whose specifiers do not all resolve is
    /// unknown — systemd resolves them, and the program "may not be a
    /// variable", so `$` and globs in it are literal. The `|` prefix runs
    /// the line in a shell.
    fn exec_line(&mut self, who: &str, unit: &str, raw: &str) -> Runs {
        let mut raw_words = words(raw);
        let grammar = if raw_words.first().is_some_and(|w| w == "|") {
            raw_words.remove(0);
            Grammar::Shell
        } else {
            Grammar::Systemd
        };
        if grammar == Grammar::Systemd {
            for command in raw_words.split(|w| w == ";") {
                if let Some(program) = command.first()
                    && !expand_specifiers(unit, program).1
                {
                    self.unknown(
                        format!(
                            "{who} may run btrbk: {program} names a specifier only that OS's \
                             systemd can resolve"
                        ),
                        None,
                    );
                }
            }
        }
        let expanded: Vec<String> = raw_words
            .iter()
            .map(|w| expand_specifiers(unit, w).0)
            .collect();
        let at = At {
            who,
            depth: 0,
            script: None,
        };
        self.words_run(at, &expanded, grammar)
    }

    /// What a command line's words run that is btrbk: btrbk named in it, and
    /// each program it launches ([`launches`]), read in turn. Every other
    /// absolute path in it is read only as a `#!` script that is there.
    fn words_run(&mut self, at: At, words: &[String], grammar: Grammar) -> Runs {
        let mut runs = self.btrbk_named(at, words);
        let mut programs = BTreeSet::new();
        for launch in launches(words, grammar) {
            match launch {
                Launch::Inline(line) => runs.extend(self.inline(at, &line)),
                Launch::Program {
                    word,
                    role,
                    literal,
                    args,
                } => {
                    runs.extend(self.program(at, &word, role, literal, &args));
                    programs.insert(word);
                }
            }
        }
        // Any other word may be a script it is given: `program` reads an
        // absolute path that is a `#!` script there, and passes over the rest.
        for word in words.iter().filter(|w| !programs.contains(*w)) {
            runs.extend(self.program(at, word, Role::Argument, false, &[]));
        }
        runs
    }

    /// btrbk named in a line's words ([`btrbk_invocations`]); a word only
    /// like its name is unknown.
    fn btrbk_named(&mut self, at: At, words: &[String]) -> Runs {
        let mut runs = Vec::new();
        for found in btrbk_invocations(words) {
            match found {
                Found::Btrbk(arg) => runs.push((at.script.map(String::from), arg)),
                Found::Like(word) => {
                    let what = match at.script {
                        Some(script) => format!("{script}, which runs {word}"),
                        None => word,
                    };
                    self.unknown(format!("{} runs {what}, named for btrbk", at.who), None);
                }
            }
        }
        runs
    }

    /// A command line passed on whole (`sh -c`), read as sh reads it.
    fn inline(&mut self, at: At, text: &str) -> Runs {
        match shell_lines(text) {
            Ok(lines) => {
                let mut runs = Vec::new();
                for line in lines {
                    runs.extend(self.words_run(at, &words(&line), Grammar::Shell));
                }
                runs
            }
            Err(why) => {
                self.unknown(
                    format!(
                        "{} may run btrbk: a command line it runs cannot be read: {why}",
                        at.who
                    ),
                    None,
                );
                Vec::new()
            }
        }
    }

    /// One program a command line launches, as `role`: found where that OS
    /// would find it, and read for what it runs — btrbk under another name,
    /// or a script, read [`MAX_SCRIPT_DEPTH`] deep at most.
    fn program(&mut self, at: At, word: &str, role: Role, literal: bool, args: &[String]) -> Runs {
        // btrbk by name is btrbk_invocations'. `.` and `source` are the
        // shell's own, their script read as the script operand: looked up
        // as programs they could only land under a mount point, and may.
        if naming(word) != Naming::Not || matches!(word, "." | "source") {
            return Vec::new();
        }
        // sh expands these when it runs: only that OS could resolve them.
        if !literal && word.contains(['$', '`', '*', '?', '[', '~']) {
            return Vec::new();
        }
        let path = if word.starts_with('/') {
            word.to_string()
        } else if role == Role::Argument {
            return Vec::new();
        } else if word.contains('/') {
            self.unknown(
                format!(
                    "{} may run btrbk through {word}: a relative path, from a directory only \
                     that OS knows",
                    at.who
                ),
                None,
            );
            return Vec::new();
        } else {
            match self.search(word) {
                Some(path) => path,
                None => return Vec::new(),
            }
        };
        match program_at(self.root, self.mounts, self.probe, &path, role) {
            Program::Btrbk => vec![(at.script.map(String::from), btrbk_config(args))],
            Program::Script(_) if at.depth == MAX_SCRIPT_DEPTH => {
                let why = format!("{path}: a script {MAX_SCRIPT_DEPTH} scripts deep, not read");
                self.unknown(format!("{} may run btrbk through {why}", at.who), None);
                Vec::new()
            }
            Program::Script(text) => self.script(at, &path, &text),
            Program::Other => Vec::new(),
            Program::Unknown(why) => {
                self.unknown(
                    format!("{} may run btrbk through {path}: {why}", at.who),
                    Some(why),
                );
                Vec::new()
            }
        }
    }

    /// Where a program named without a path is found ([`MERGED_PATH`], or
    /// [`SPLIT_PATH`] where `usr/sbin` is a directory of its own): the first
    /// directory that has it or may have it ([`Mounts::hides`]); `None`
    /// when none does — the command fails.
    fn search(&self, name: &str) -> Option<String> {
        let dirs: &[&str] = match entry_at(self.root, "usr/sbin") {
            Entry::Dir => &SPLIT_PATH,
            _ => &MERGED_PATH,
        };
        dirs.iter().find_map(|dir| {
            let rel = format!("{dir}/{name}");
            let there = !matches!(locate(self.root, &rel, self.probe), Located::Absent { .. })
                || self.mounts.hides(&rel).is_some();
            there.then(|| format!("/{rel}"))
        })
    }

    /// A script's lines, read as sh reads them ([`shell_lines`]), one script
    /// deeper than `at`, each script once: what one running itself runs ends
    /// there.
    fn script(&mut self, at: At, path: &str, text: &str) -> Runs {
        if let Some(runs) = self.scripts.get(path) {
            return runs.clone();
        }
        self.scripts.insert(path.to_string(), Vec::new());
        let inside = At {
            who: at.who,
            depth: at.depth + 1,
            script: Some(path),
        };
        let mut runs = Vec::new();
        match shell_lines(text) {
            Ok(lines) => {
                for line in lines {
                    runs.extend(self.words_run(inside, &words(&line), Grammar::Shell));
                }
            }
            Err(why) => self.unknown(
                format!("{} may run btrbk through {path}: {path}: {why}", at.who),
                None,
            ),
        }
        self.scripts.insert(path.to_string(), runs.clone());
        runs
    }

    fn runner(
        &mut self,
        source: &str,
        via: Option<&str>,
        script: Option<String>,
        when: &str,
        arg: ConfigArg,
    ) {
        let (config, outcome) = match arg {
            ConfigArg::Default => (None, default_outcome(self.default_config)),
            ConfigArg::Path(path) => {
                let outcome = path_outcome(self.root, self.mounts, self.probe, &path);
                (Some(path), outcome)
            }
            ConfigArg::Unresolved(raw) => {
                let why = format!("-c {raw} cannot be resolved without running that OS");
                (Some(raw), Outcome::Unknown(why))
            }
        };
        let runner = BtrbkRunner {
            source: source.to_string(),
            via: via.map(String::from),
            script,
            when: when.to_string(),
            config,
            config_present: outcome.present(),
        };
        if !self.runners.iter().any(|(r, _)| *r == runner) {
            self.runners.push((runner, outcome));
        }
    }

    /// Cron's tables and scripts, read when a cron daemon is among the units.
    /// A table line says when by its schedule (`@reboot`: at boot); anacron's
    /// table, and the script directories it names, run when anacron catches
    /// up on what the system missed while it was off. A table or directory
    /// under a mount point may hold what cron sees there: unknown.
    fn cron(&mut self, daemon: &str) {
        let mut files: Vec<String> = Vec::new();
        for rel in CRON_FILES.iter().chain(CRON_DIRS.iter()) {
            if let Some(why) = self.mounts.covers(rel) {
                self.unknown(format!("cron ({daemon}) is enabled and {why}"), None);
                continue;
            }
            if CRON_FILES.contains(rel) {
                files.push(rel.to_string());
                continue;
            }
            match locate(self.root, rel, self.probe) {
                Located::Dir(at) => match list_dir(self.root, &at) {
                    Listing::Names(names) => {
                        files.extend(names.into_iter().map(|n| format!("{rel}/{n}")));
                    }
                    Listing::Absent => {}
                    Listing::Unreadable(why) => {
                        self.unknown(format!("cron ({daemon}) is enabled and {why}"), Some(why));
                    }
                },
                Located::Absent { rel: at, .. } => {
                    if let Some(why) = self.mounts.hides(&at) {
                        self.unknown(format!("cron ({daemon}) is enabled and {why}"), None);
                    }
                }
                Located::Unread { why, .. } | Located::Unknown(why) => {
                    self.unknown(format!("cron ({daemon}) is enabled and {why}"), Some(why));
                }
                Located::File(..) | Located::Node(_) | Located::Masked => {}
            }
        }
        let mut texts = Vec::new();
        for rel in files {
            match read_file(self.root, &rel, self.probe) {
                FileRead::Text(text) => texts.push((rel, text)),
                FileRead::Nothing | FileRead::NotFile => {}
                FileRead::Unread(why) | FileRead::Unknown(why) => {
                    self.unknown(format!("cron ({daemon}) is enabled and {why}"), Some(why));
                }
            }
        }
        let anacron: Vec<String> =
            texts
                .iter()
                .find(|(rel, _)| rel == ANACRONTAB)
                .map_or_else(Vec::new, |(_, text)| {
                    text.lines()
                        .filter(|l| !l.trim_start().starts_with('#'))
                        .map(String::from)
                        .collect()
                });
        let catch_up = "soon after boot (anacron catch-up)";
        for (rel, text) in &texts {
            let who = format!("{rel} (cron, {daemon})");
            let script_dir = CRON_SCRIPT_DIRS.iter().find(|d| under(rel, d));
            let lines: Vec<(String, &str)> = if let Some(dir) = script_dir {
                let ran_by_anacron = ANACRON_DIRS
                    .iter()
                    .any(|d| dir.ends_with(d) && anacron.iter().any(|l| l.contains(d)));
                let when = if ran_by_anacron {
                    catch_up
                } else {
                    "on its cron schedule"
                };
                match shell_lines(text) {
                    Ok(lines) => lines.into_iter().map(|l| (l, when)).collect(),
                    Err(why) => {
                        self.unknown(format!("{who} may run btrbk: {rel}: {why}"), None);
                        Vec::new()
                    }
                }
            } else {
                let table = if rel == ANACRONTAB {
                    Table::Anacron
                } else if rel == "etc/crontab" || under(rel, "etc/cron.d") {
                    Table::System
                } else {
                    Table::User
                };
                let mut lines = Vec::new();
                for line in text.lines() {
                    let Some((command, reboot)) = cron_command(line, table) else {
                        continue;
                    };
                    let when = if rel == ANACRONTAB {
                        catch_up
                    } else if reboot {
                        "at boot (cron @reboot)"
                    } else {
                        "on its cron schedule"
                    };
                    match shell_lines(command) {
                        Ok(commands) => lines.extend(commands.into_iter().map(|c| (c, when))),
                        Err(why) => self.unknown(
                            format!("{who} may run btrbk: a command in {rel}: {why}"),
                            None,
                        ),
                    }
                }
                lines
            };
            for (line, when) in lines {
                let at = At {
                    who: &who,
                    depth: 0,
                    script: None,
                };
                let runs = self.words_run(at, &words(&line), Grammar::Shell);
                for (script, arg) in runs {
                    self.runner(rel, Some(daemon), script, when, arg);
                }
            }
        }
    }

    /// The verdict, and what it rests on.
    fn verdict(self) -> (BtrbkAtBoot, Vec<String>) {
        let mut will = Vec::new();
        let mut may = self.unknowns;
        let mut ruled_out = Vec::new();
        for (runner, outcome) in &self.runners {
            let text = runner_text(runner, outcome);
            match outcome {
                Outcome::Present(_) => will.push(text),
                Outcome::Unknown(_) => may.push(text),
                Outcome::Absent(_) => ruled_out.push(text),
            }
        }
        let (verdict, reasons) = if !will.is_empty() {
            (BootVerdict::Will, will)
        } else if !may.is_empty() {
            (BootVerdict::May, may)
        } else {
            (BootVerdict::No, ruled_out)
        };
        let at_boot = BtrbkAtBoot {
            verdict,
            reasons,
            runners: self.runners.into_iter().map(|(r, _)| r).collect(),
        };
        (at_boot, self.problems)
    }
}

/// The units `read` starts, for a reach `pull`: all of them on its first
/// reach; on a sooner one only those that take their [`When`] from it.
fn starts_of(read: &UnitRead, pull: &Pull, sooner: bool) -> Vec<Pull> {
    if let Some(target) = &read.alias {
        let mut aliases = pull.aliases.clone();
        aliases.push(pull.name.clone());
        return vec![Pull {
            name: target.clone(),
            via: pull.via.clone(),
            when: pull.when.clone(),
            named: pull.named,
            aliases,
        }];
    }
    read.starts
        .iter()
        .filter(|s| !sooner || matches!(s.how, How::Inherit))
        .map(|s| Pull {
            name: s.name.clone(),
            via: Some(pull.name.clone()),
            when: match &s.how {
                How::Inherit => pull.when.clone(),
                How::Fixed(when) => when.clone(),
                How::Fails => When::Fails(pull.name.clone()),
                How::Succeeds => When::Succeeds(pull.name.clone()),
            },
            named: s.named,
            aliases: Vec::new(),
        })
        .collect()
}

/// A cron table, by the fields before a line's command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Table {
    /// `etc/crontab` and `etc/cron.d`: five time fields, then a user.
    System,
    /// A user's, under `var/spool/cron`: five time fields.
    User,
    /// `etc/anacrontab`: a period, a delay, a job name.
    Anacron,
}

/// The command of a cron table line past its table's fields, up to an
/// unescaped `%` — cron's newline, after which comes the command's input;
/// a backslash escapes the next character, as cronie's do_command.c reads
/// it — and whether it runs `@reboot`. `None` for a blank line, a comment,
/// an environment setting, or a line with no command.
fn cron_command(line: &str, table: Table) -> Option<(&str, bool)> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    // `NAME=value`, or `NAME = value`.
    if trimmed
        .split_once('=')
        .is_some_and(|(name, _)| is_assignment(&format!("{}=", name.trim())))
    {
        return None;
    }
    let first = trimmed.split_whitespace().next()?;
    // An @keyword stands for the five time fields; anacrontab's period is
    // one field either way.
    let fields = match (table, first.starts_with('@')) {
        (Table::Anacron, _) => 3,
        (Table::System, false) => 6,
        (Table::System, true) => 2,
        (Table::User, false) => 5,
        (Table::User, true) => 1,
    };
    let mut rest = trimmed;
    for _ in 0..fields {
        let field_end = rest.find(char::is_whitespace)?;
        rest = rest[field_end..].trim_start();
    }
    let mut end = rest.len();
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '%' if !escaped => {
                end = i;
                break;
            }
            _ => escaped = false,
        }
    }
    let command = rest[..end].trim_end();
    (!command.is_empty()).then_some((command, first == "@reboot"))
}

/// `btrbk.timer starts btrbk.service, which runs btrbk straight after boot
/// (Persistent catch-up), with /etc/btrbk/btrbk.conf present`; with
/// `through SCRIPT` after `runs btrbk` when a script it names does.
fn runner_text(runner: &BtrbkRunner, outcome: &Outcome) -> String {
    let what = match &runner.via {
        Some(daemon) if runner.source.contains('/') => {
            format!("{} (cron, {daemon}) runs btrbk", runner.source)
        }
        Some(via) => format!("{via} starts {}, which runs btrbk", runner.source),
        None => format!("{} runs btrbk", runner.source),
    };
    let through = runner
        .script
        .as_ref()
        .map_or_else(String::new, |script| format!(" through {script}"));
    let config = match outcome {
        Outcome::Present(path) => format!("with {path} present"),
        Outcome::Absent(what) => format!("but {what}, so it stops at once"),
        Outcome::Unknown(why) => format!("its config unknown: {why}"),
    };
    format!("{what}{through} {}, {config}", runner.when)
}

/// What [`read_with`] found in one OS root.
pub(super) struct Reading {
    pub units: EnabledUnits,
    pub config: BtrbkConfig,
    pub at_boot: BtrbkAtBoot,
    /// What could not be read, for the drive's `problems`, each once.
    pub problems: Vec<String>,
}

/// Read what this OS starts at boot and whether any of it runs btrbk,
/// reading a link only where `probe` says that records nothing.
pub(super) fn read_with(root: &Path, probe: MountFlags) -> Reading {
    read_within(root, probe, MAX_PULLS)
}

/// [`read_with`], following at most `pulls` starts.
fn read_within(root: &Path, probe: MountFlags, pulls: usize) -> Reading {
    let trees: Vec<Tree> = UNIT_TREES
        .iter()
        .map(|rel| Tree {
            rel,
            listing: list_dir(root, rel),
        })
        .collect();
    let units = read_enabled_units(root, &trees);
    let mounts = read_mounts(root, &trees, &units, probe);
    let config = read_btrbk_config(root, &mounts, probe);
    let owned = match &units {
        EnabledUnits::Listed { units } => dependency_dirs(units),
        EnabledUnits::Unreadable { .. } => BTreeMap::new(),
    };
    let mut findings = Findings {
        root,
        trees: &trees,
        mounts: &mounts,
        probe,
        default_config: &config,
        owned,
        units: BTreeMap::new(),
        scripts: BTreeMap::new(),
        capped: false,
        pulls: 0..pulls,
        runners: Vec::new(),
        unknowns: Vec::new(),
        problems: Vec::new(),
    };
    let mut problems = Vec::new();
    if let Some(why) = &mounts.fstab {
        problems.push(why.clone());
    }
    match &units {
        EnabledUnits::Listed { units } => findings.walk(units),
        EnabledUnits::Unreadable { reason } => {
            findings
                .unknowns
                .push(format!("enabled units unknown: {reason}"));
            problems.push(reason.clone());
        }
    }
    if let BtrbkConfig::Unreadable { reason } = &config {
        problems.push(reason.clone());
    }
    let (at_boot, found) = findings.verdict();
    for problem in found {
        if !problems.contains(&problem) {
            problems.push(problem);
        }
    }
    Reading {
        units,
        config,
        at_boot,
        problems,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_os::{mount_flags, testutil};
    use std::os::unix::fs::{PermissionsExt, symlink};

    const ETC: &str = "etc/systemd/system";
    const LOCAL: &str = "usr/local/lib/systemd/system";
    const VENDOR: &str = "usr/lib/systemd/system";

    /// btrbk's own units, as Arch's btrbk 0.32 package ships them.
    const BTRBK_SERVICE: &str = "[Unit]\nDescription=btrbk backup\nDocumentation=man:btrbk(1)\n\n\
                                 [Service]\nType=oneshot\nExecStart=/usr/bin/btrbk run\n";
    const BTRBK_TIMER: &str = "[Unit]\nDescription=btrbk daily backup\n\n[Timer]\n\
                               OnCalendar=daily\nAccuracySec=10min\nPersistent=true\n\n\
                               [Install]\nWantedBy=timers.target\n";
    const CONF: &str = "volume /mnt/btr_pool\n  target /mnt/backup\n  subvolume @\n";
    /// An OS that mounts nothing else over its root.
    const UNMOUNTED: Mounts = Mounts {
        points: Vec::new(),
        fstab: None,
    };
    /// Links read, as on a `noatime` mount — where every DAS target is.
    const FOLLOW: MountFlags = |_| Some(libc::ST_NOATIME);
    /// Links never read, as on a mount that records access times.
    const REFUSE: MountFlags = |_| Some(libc::ST_RELATIME);

    /// The reading, links read as production reads them ([`FOLLOW`]).
    fn read(root: &Path) -> Reading {
        read_with(root, FOLLOW)
    }

    /// The reading where links cannot be read ([`REFUSE`]).
    fn read_refusing(root: &Path) -> Reading {
        read_with(root, REFUSE)
    }

    /// Why the link `rel` was not read under [`REFUSE`].
    fn unread(rel: &str) -> String {
        format!("{rel}: a link, not read: this mount records access times (mount it noatime)")
    }
    const PRESENT: &str = "with /etc/btrbk/btrbk.conf present";

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn link(root: &Path, rel: &str, target: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(target, path).unwrap();
    }

    /// `systemctl enable`'s link: the unit's name in `tree/dir`, pointing
    /// where nothing is, so anything that follows it fails.
    fn enable(root: &Path, tree: &str, dir: &str, unit: &str) {
        link(
            root,
            &format!("{tree}/{dir}/{unit}"),
            &format!("/nonexistent/das-test/{unit}"),
        );
    }

    fn unit(root: &Path, tree: &str, name: &str, text: &str) {
        write(root, &format!("{tree}/{name}"), text);
    }

    fn package_btrbk(root: &Path) {
        unit(root, VENDOR, "btrbk.service", BTRBK_SERVICE);
        unit(root, VENDOR, "btrbk.timer", BTRBK_TIMER);
    }

    fn configure(root: &Path) {
        write(root, "etc/btrbk/btrbk.conf", CONF);
    }

    fn at_boot(root: &Path) -> BtrbkAtBoot {
        read(root).at_boot
    }

    fn nothing() -> BtrbkAtBoot {
        BtrbkAtBoot {
            verdict: BootVerdict::No,
            reasons: Vec::new(),
            runners: Vec::new(),
        }
    }

    fn runner(
        source: &str,
        via: Option<&str>,
        when: &str,
        config: Option<&str>,
        present: Option<bool>,
    ) -> BtrbkRunner {
        BtrbkRunner {
            source: source.into(),
            via: via.map(String::from),
            script: None,
            when: when.into(),
            config: config.map(String::from),
            config_present: present,
        }
    }

    /// `r`, found in a line of `script` rather than in its command.
    fn through(r: BtrbkRunner, script: &str) -> BtrbkRunner {
        BtrbkRunner {
            script: Some(script.into()),
            ..r
        }
    }

    /// A `#!` script at `rel`, its lines after the interpreter line.
    fn script(root: &Path, rel: &str, body: &str) {
        write(root, rel, &format!("#!/bin/sh\n{body}"));
    }

    /// A FIFO at `rel`, with a writer standing by: a read that does open it
    /// gets an end of file instead of hanging the test. The guard releases
    /// the writer if nothing did.
    struct Fifo {
        path: std::path::PathBuf,
        writer: Option<std::thread::JoinHandle<()>>,
    }

    impl Fifo {
        fn new(root: &Path, rel: &str) -> Fifo {
            let path = root.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            // SAFETY: a valid NUL-terminated path; mkfifo only creates the node.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            let writer = std::thread::spawn({
                let path = path.clone();
                move || drop(fs::OpenOptions::new().write(true).open(&path).unwrap())
            });
            Fifo {
                path,
                writer: Some(writer),
            }
        }
    }

    impl Drop for Fifo {
        fn drop(&mut self) {
            use std::os::unix::fs::OpenOptionsExt;
            let _reader = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.path);
            if let Some(writer) = self.writer.take() {
                writer.join().unwrap();
            }
        }
    }

    // ---- systemd ---------------------------------------------------------

    #[test]
    fn the_packaged_btrbk_timer_enabled_beside_a_config_will_run_and_says_when() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        enable(root, ETC, "timers.target.wants", "btrbk.timer");
        configure(root);
        let next = "at its next scheduled time after boot";
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.reasons,
            [format!(
                "btrbk.timer starts btrbk.service, which runs btrbk {next}, {PRESENT}"
            )]
        );
        assert_eq!(
            b.runners,
            [runner(
                "btrbk.service",
                Some("btrbk.timer"),
                next,
                None,
                Some(true)
            )]
        );
        // A stamp says it ran before: the missed runs are caught up at once.
        write(root, "var/lib/systemd/timers/stamp-btrbk.timer", "");
        assert_eq!(
            at_boot(root).reasons,
            [format!(
                "btrbk.timer starts btrbk.service, which runs btrbk straight after boot \
                 (Persistent catch-up), {PRESENT}"
            )]
        );
        // Without a config it stops at once: no.
        fs::remove_file(root.join("etc/btrbk/btrbk.conf")).unwrap();
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::No);
        assert_eq!(
            b.reasons,
            [
                "btrbk.timer starts btrbk.service, which runs btrbk straight after boot \
                 (Persistent catch-up), but /etc/btrbk.conf and /etc/btrbk/btrbk.conf are \
                 absent, so it stops at once"
            ]
        );
        assert_eq!(b.runners[0].config_present, Some(false));
        // The package alone, not enabled, runs nothing.
        configure(root);
        fs::remove_file(root.join("etc/systemd/system/timers.target.wants/btrbk.timer")).unwrap();
        assert_eq!(at_boot(root), nothing());
    }

    #[test]
    fn a_service_a_boot_target_pulls_in_runs_btrbk_at_every_boot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        enable(root, ETC, "multi-user.target.wants", "btrbk.service");
        configure(root);
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.reasons,
            [format!("btrbk.service runs btrbk at every boot, {PRESENT}")]
        );
        // Pulled in only at shutdown it still runs — then.
        fs::remove_file(root.join("etc/systemd/system/multi-user.target.wants/btrbk.service"))
            .unwrap();
        enable(root, ETC, "shutdown.target.wants", "btrbk.service");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.reasons,
            [format!(
                "btrbk.service runs btrbk when shutdown.target starts, {PRESENT}"
            )]
        );
        // Any boot target among them makes it every boot.
        enable(root, ETC, "graphical.target.wants", "btrbk.service");
        assert_eq!(at_boot(root).runners[0].when, "at every boot");
        // A socket's commands run when it is set up at boot.
        unit(
            root,
            ETC,
            "snap.socket",
            "[Socket]\nListenStream=/run/snap.sock\nExecStartPre=/usr/bin/btrbk run\n",
        );
        enable(root, ETC, "sockets.target.wants", "snap.socket");
        assert!(
            at_boot(root)
                .reasons
                .contains(&format!("snap.socket runs btrbk at every boot, {PRESENT}")),
        );
    }

    #[test]
    fn a_renamed_timer_whose_service_runs_btrbk_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        unit(
            root,
            ETC,
            "nightly.timer",
            "[Timer]\nOnCalendar=*-*-* 02:00\nUnit=snap-backup.service\n",
        );
        unit(
            root,
            ETC,
            "snap-backup.service",
            "[Service]\nType=oneshot\n\
             ExecStart=/usr/bin/nice -n 19 /usr/bin/ionice -c3 /usr/bin/btrbk -q run\n",
        );
        enable(root, ETC, "timers.target.wants", "nightly.timer");
        configure(root);
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.reasons,
            [format!(
                "nightly.timer starts snap-backup.service, which runs btrbk at its next \
                 scheduled time after boot, {PRESENT}"
            )]
        );
        // Without Unit=, a timer starts the service of its own name.
        unit(root, ETC, "weekly.timer", "[Timer]\nOnBootSec=15min\n");
        unit(
            root,
            ETC,
            "weekly.service",
            "[Service]\nExecStart=/bin/sh -c 'exec btrbk run'\n",
        );
        enable(root, ETC, "timers.target.wants", "weekly.timer");
        assert!(at_boot(root).reasons.contains(&format!(
            "weekly.timer starts weekly.service, which runs btrbk soon after boot \
                 (OnBootSec=, OnStartupSec= or OnActiveSec=), {PRESENT}"
        )),);
        // A path unit starts its service when its condition is met.
        unit(
            root,
            ETC,
            "watch.path",
            "[Path]\nPathExists=/tmp/go\nUnit=snap-backup.service\n",
        );
        enable(root, ETC, "paths.target.wants", "watch.path");
        assert!(at_boot(root).reasons.contains(&format!(
            "watch.path starts snap-backup.service, which runs btrbk when its path \
                 condition is met, {PRESENT}"
        )),);
    }

    #[test]
    fn a_drop_in_that_resets_exec_start_away_from_btrbk_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        enable(root, ETC, "multi-user.target.wants", "btrbk.service");
        configure(root);
        let drop_in = "etc/systemd/system/btrbk.service.d/override.conf";
        write(
            root,
            drop_in,
            "[Service]\nExecStart=\nExecStart=/usr/bin/true\n",
        );
        assert_eq!(at_boot(root), nothing(), "though its name is btrbk's");
        // Without the reset a second ExecStart= only adds to the first.
        write(root, drop_in, "[Service]\nExecStart=/usr/bin/true\n");
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
        // A reset in another section resets nothing.
        write(root, drop_in, "[Unit]\nExecStart=\n");
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
        // A drop-in that is not a .conf is not one.
        fs::remove_file(root.join(drop_in)).unwrap();
        write(
            root,
            "etc/systemd/system/btrbk.service.d/override.conf.bak",
            "[Service]\nExecStart=\nExecStart=/usr/bin/true\n",
        );
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
    }

    #[test]
    fn a_config_passed_with_c_is_the_one_checked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        enable(root, ETC, "multi-user.target.wants", "btrbk.service");
        configure(root); // btrbk's default config, which -c overrides
        let drop_in = "etc/systemd/system/btrbk.service.d/override.conf";
        let exec = |line: &str| {
            write(
                root,
                drop_in,
                &format!("[Service]\nExecStart=\nExecStart={line}\n"),
            );
        };
        exec("/usr/bin/btrbk -c /opt/x.conf run");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::No);
        assert_eq!(
            b.reasons,
            [
                "btrbk.service runs btrbk at every boot, but /opt/x.conf is absent, so it stops at once"
            ]
        );
        assert_eq!(
            b.runners,
            [runner(
                "btrbk.service",
                None,
                "at every boot",
                Some("/opt/x.conf"),
                Some(false)
            )]
        );
        write(root, "opt/x.conf", CONF);
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.reasons,
            ["btrbk.service runs btrbk at every boot, with /opt/x.conf present"]
        );
        for line in [
            "/usr/bin/btrbk --config=/opt/x.conf run",
            "/usr/bin/btrbk --config /opt/x.conf run",
            "/usr/bin/btrbk -c/opt/x.conf run",
            "/bin/sh -c 'btrbk -c /opt/x.conf run'",
        ] {
            exec(line);
            let b = at_boot(root);
            assert_eq!(
                (b.verdict, b.runners[0].config.as_deref()),
                (BootVerdict::Will, Some("/opt/x.conf")),
                "{line}"
            );
        }
        // Not resolvable here: it may run.
        exec("/usr/bin/btrbk -c ${CONF} run");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [
                "btrbk.service runs btrbk at every boot, its config unknown: -c ${CONF} cannot \
                 be resolved without running that OS"
            ]
        );
        // A config that is a link: unknown unread; read, what it leads to.
        exec("/usr/bin/btrbk -c /opt/x.conf run");
        fs::remove_file(root.join("opt/x.conf")).unwrap();
        link(root, "opt/x.conf", "/etc/hostname");
        let b = read_refusing(root).at_boot;
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(b.runners[0].config_present, None);
        assert_eq!(at_boot(root).verdict, BootVerdict::No, "leads nowhere");
        write(root, "etc/hostname", "recovery\n");
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
    }

    #[test]
    fn a_vendor_enabled_unit_is_found_and_recorded_with_its_directory() {
        for tree in [VENDOR, LOCAL] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            package_btrbk(root);
            enable(root, tree, "timers.target.wants", "btrbk.timer");
            configure(root);
            let r = read(root);
            assert_eq!(
                r.units,
                EnabledUnits::Listed {
                    units: vec![EnabledUnit {
                        name: "btrbk.timer".into(),
                        dirs: vec![format!("{tree}/timers.target.wants")],
                    }]
                }
            );
            assert_eq!(r.at_boot.verdict, BootVerdict::Will, "{tree}");
        }
    }

    #[test]
    fn every_enabled_name_is_kept_once_with_every_directory_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        enable(root, ETC, "multi-user.target.wants", "sshd.service");
        enable(root, ETC, "timers.target.wants", "fstrim.timer");
        enable(root, LOCAL, "multi-user.target.wants", "local.service");
        enable(root, VENDOR, "timers.target.wants", "fstrim.timer");
        enable(
            root,
            VENDOR,
            "sockets.target.wants",
            "systemd-journald.socket",
        );
        enable(root, VENDOR, "local-fs.target.requires", "tmp.mount");
        enable(root, VENDOR, "graphical.target.upholds", "upheld.service");
        fs::create_dir_all(root.join(VENDOR).join("empty.target.wants")).unwrap();
        write(root, &format!("{ETC}/odd.target.wants"), "a file");
        // An alias, not a dependency directory.
        link(
            root,
            &format!("{ETC}/default.target"),
            "/usr/lib/systemd/system/graphical.target",
        );
        let unit = |name: &str, dirs: &[&str]| EnabledUnit {
            name: name.into(),
            dirs: dirs.iter().map(|d| d.to_string()).collect(),
        };
        let r = read(root);
        assert_eq!(
            r.units,
            EnabledUnits::Listed {
                units: vec![
                    unit(
                        "fstrim.timer",
                        &[
                            "etc/systemd/system/timers.target.wants",
                            "usr/lib/systemd/system/timers.target.wants"
                        ]
                    ),
                    unit(
                        "local.service",
                        &["usr/local/lib/systemd/system/multi-user.target.wants"]
                    ),
                    unit(
                        "sshd.service",
                        &["etc/systemd/system/multi-user.target.wants"]
                    ),
                    unit(
                        "systemd-journald.socket",
                        &["usr/lib/systemd/system/sockets.target.wants"]
                    ),
                    unit(
                        "tmp.mount",
                        &["usr/lib/systemd/system/local-fs.target.requires"]
                    ),
                    unit(
                        "upheld.service",
                        &["usr/lib/systemd/system/graphical.target.upholds"]
                    ),
                ]
            }
        );
        // No unit file is there (dangling enablements): nothing runs btrbk.
        assert_eq!(r.at_boot, nothing());
        assert!(r.problems.is_empty(), "{:?}", r.problems);
    }

    #[test]
    fn a_linked_or_masked_unit_may_run_btrbk_where_its_link_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configure(root);
        // `systemctl mask`: a link to /dev/null, which only reading the link
        // tells from `systemctl link`'s link to a unit file elsewhere.
        enable(root, ETC, "multi-user.target.wants", "foo.service");
        unit(
            root,
            VENDOR,
            "foo.service",
            "[Service]\nExecStart=/usr/bin/foo\n",
        );
        link(root, &format!("{ETC}/foo.service"), "/dev/null");
        let masked = root.join(ETC).join("foo.service");
        let why = unread("etc/systemd/system/foo.service");
        let r = read_refusing(root);
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
        assert_eq!(
            r.at_boot.reasons,
            [format!("foo.service may run btrbk: {why}")]
        );
        assert_eq!(r.problems, [why]);
        // Read, the link says masked: it runs nothing, whatever the vendor's does.
        unit(
            root,
            VENDOR,
            "foo.service",
            "[Service]\nExecStart=/usr/bin/btrbk run\n",
        );
        assert_eq!(at_boot(root), nothing(), "masked");
        unit(
            root,
            VENDOR,
            "foo.service",
            "[Service]\nExecStart=/usr/bin/foo\n",
        );
        fs::remove_file(&masked).unwrap();
        assert_eq!(
            at_boot(root),
            nothing(),
            "the vendor file runs foo, not btrbk"
        );
        // A package's alias (Arch: dbus.service → dbus-broker.service).
        enable(root, VENDOR, "multi-user.target.wants", "dbus.service");
        link(
            root,
            &format!("{VENDOR}/dbus.service"),
            "dbus-broker.service",
        );
        unit(
            root,
            VENDOR,
            "dbus-broker.service",
            "[Service]\nExecStart=/usr/bin/dbus-broker-launch\n",
        );
        let r = read_refusing(root);
        assert_eq!(r.at_boot, nothing());
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert_eq!(at_boot(root), nothing(), "read, it is dbus-broker.service");
        // ...unless its name is btrbk's.
        enable(
            root,
            VENDOR,
            "multi-user.target.wants",
            "btrbk-alias.service",
        );
        link(
            root,
            &format!("{VENDOR}/btrbk-alias.service"),
            "btrbk.service",
        );
        let b = read_refusing(root).at_boot;
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [
                "btrbk-alias.service is named for btrbk, and its unit file is a package's alias, not followed"
            ]
        );
        // Read, it is btrbk.service — not there, and named for btrbk: may.
        assert_eq!(
            at_boot(root).reasons,
            ["btrbk.service is named for btrbk, and its unit file was not found"]
        );
        unit(root, VENDOR, "btrbk.service", BTRBK_SERVICE);
        assert_eq!(
            at_boot(root).runners,
            [runner(
                "btrbk.service",
                None,
                "at every boot",
                None,
                Some(true)
            )],
            "an alias is the unit it names, by that unit's own name"
        );
    }

    #[test]
    fn a_template_instance_reads_its_template_and_both_drop_in_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        enable(root, ETC, "multi-user.target.wants", "backup@daily.service");
        unit(
            root,
            VENDOR,
            "backup@.service",
            "[Service]\nExecStart=/usr/bin/btrbk -c /etc/btrbk/%i.conf run\n",
        );
        // %i is the instance: the config is /etc/btrbk/daily.conf, absent.
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::No);
        assert_eq!(
            b.reasons,
            [
                "backup@daily.service runs btrbk at every boot, but /etc/btrbk/daily.conf is \
                 absent, so it stops at once"
            ]
        );
        assert_eq!(
            b.runners[0].config.as_deref(),
            Some("/etc/btrbk/daily.conf")
        );
        // A specifier only that OS resolves leaves the config unknown.
        unit(
            root,
            VENDOR,
            "backup@.service",
            "[Service]\nExecStart=/usr/bin/btrbk -c /etc/btrbk/%H.conf run\n",
        );
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [
                "backup@daily.service runs btrbk at every boot, its config unknown: -c \
                 /etc/btrbk/%H.conf cannot be resolved without running that OS"
            ]
        );
        // The instance's own drop-in.
        let instance = "etc/systemd/system/backup@daily.service.d/50-conf.conf";
        write(
            root,
            instance,
            "[Service]\nExecStart=\nExecStart=/usr/bin/btrbk -c /etc/btrbk/daily.conf run\n",
        );
        write(root, "etc/btrbk/daily.conf", CONF);
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.reasons,
            ["backup@daily.service runs btrbk at every boot, with /etc/btrbk/daily.conf present"]
        );
        // The template's file of the same name yields to the instance's.
        let template = "usr/lib/systemd/system/backup@.service.d/50-conf.conf";
        write(
            root,
            template,
            "[Service]\nExecStart=\nExecStart=/usr/bin/true\n",
        );
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
        // Alone, the template's applies to every instance.
        fs::remove_file(root.join(instance)).unwrap();
        assert_eq!(at_boot(root), nothing());
    }

    #[test]
    fn a_dangling_enablement_runs_nothing_unless_it_is_named_for_btrbk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configure(root);
        // A package removed, its link left behind.
        enable(root, ETC, "multi-user.target.wants", "smb.service");
        assert_eq!(at_boot(root), nothing());
        enable(root, ETC, "timers.target.wants", "btrbk-hourly.timer");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            ["btrbk-hourly.timer is named for btrbk, and its unit file was not found"]
        );
        // A timer found whose service is not: named for btrbk, so may.
        unit(
            root,
            ETC,
            "btrbk-hourly.timer",
            "[Timer]\nOnCalendar=hourly\n",
        );
        assert_eq!(
            at_boot(root).reasons,
            [
                "btrbk-hourly.timer starts btrbk-hourly.service, which is named for btrbk, and its \
                 unit file was not found"
            ]
        );
        // Another kind of unit named for btrbk.
        fs::remove_dir_all(root.join(ETC)).unwrap();
        enable(root, ETC, "multi-user.target.wants", "btrbk.target");
        assert_eq!(
            at_boot(root).reasons,
            ["btrbk.target is named for btrbk, and its unit file was not found"]
        );
    }

    #[test]
    fn drop_ins_apply_in_file_name_order_and_the_highest_tree_wins_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        enable(root, ETC, "multi-user.target.wants", "job.service");
        unit(
            root,
            VENDOR,
            "job.service",
            "[Service]\nExecStart=/usr/bin/job\n",
        );
        configure(root);
        write(
            root,
            "usr/lib/systemd/system/job.service.d/20-run.conf",
            "[Service]\nExecStartPost=/usr/bin/btrbk run\n",
        );
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
        // The same name under /etc replaces the vendor's.
        write(
            root,
            "etc/systemd/system/job.service.d/20-run.conf",
            "[Service]\nExecStartPost=/usr/bin/true\n",
        );
        assert_eq!(at_boot(root), nothing());
        // A later name applies after an earlier one, whatever its tree.
        write(
            root,
            "etc/systemd/system/job.service.d/10-clear.conf",
            "[Service]\nExecStartPost=\n",
        );
        write(
            root,
            "usr/lib/systemd/system/job.service.d/30-again.conf",
            "[Service]\nExecStopPost=/usr/bin/btrbk run\n",
        );
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
        // The type's drop-ins apply to every unit of the type.
        fs::remove_file(root.join("usr/lib/systemd/system/job.service.d/30-again.conf")).unwrap();
        assert_eq!(at_boot(root), nothing());
        write(
            root,
            "etc/systemd/system/service.d/99-snap.conf",
            "[Service]\nExecStartPost=/usr/bin/btrbk run\n",
        );
        assert_eq!(
            at_boot(root).reasons,
            [format!("job.service runs btrbk at every boot, {PRESENT}")]
        );
        // A masked drop-in (a link): unknown unread; read, empty.
        fs::remove_file(root.join("etc/systemd/system/service.d/99-snap.conf")).unwrap();
        link(
            root,
            "etc/systemd/system/job.service.d/40-mask.conf",
            "/dev/null",
        );
        assert_eq!(read_refusing(root).at_boot.verdict, BootVerdict::May);
        assert_eq!(at_boot(root), nothing());
    }

    #[test]
    fn drop_in_directories_cover_the_template_the_dash_prefixes_and_the_type() {
        assert_eq!(
            drop_in_dirs("foo-bar-baz.service"),
            [
                "foo-bar-baz.service.d",
                "foo-bar-.service.d",
                "foo-.service.d",
                "service.d"
            ]
        );
        assert_eq!(
            drop_in_dirs("backup@daily.service"),
            ["backup@daily.service.d", "backup@.service.d", "service.d"]
        );
        assert_eq!(drop_in_dirs("btrbk.timer"), ["btrbk.timer.d", "timer.d"]);
        assert_eq!(
            template_of("backup@daily.service").as_deref(),
            Some("backup@.service")
        );
        assert_eq!(
            template_of("dirmngr@etc-pacman.d-gnupg.socket").as_deref(),
            Some("dirmngr@.socket")
        );
        assert_eq!(template_of("backup@.service"), None);
        assert_eq!(template_of("plain.service"), None);
        // Through read(): a dash prefix's drop-in.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        enable(root, ETC, "multi-user.target.wants", "snap-daily.service");
        unit(
            root,
            VENDOR,
            "snap-daily.service",
            "[Service]\nExecStart=/usr/bin/snap\n",
        );
        configure(root);
        write(
            root,
            "etc/systemd/system/snap-.service.d/btrbk.conf",
            "[Service]\nExecStartPost=/usr/bin/btrbk run\n",
        );
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
    }

    // ---- cron ------------------------------------------------------------

    fn cron_root(root: &Path) {
        unit(
            root,
            VENDOR,
            "cronie.service",
            "[Service]\nExecStart=/usr/bin/crond -n\n",
        );
        enable(root, ETC, "multi-user.target.wants", "cronie.service");
        configure(root);
    }

    #[test]
    fn a_cron_line_s_command_follows_its_table_s_fields_to_an_unescaped_percent() {
        use Table::{Anacron, System, User};
        for (line, table, want) in [
            ("0 3 * * * root a b", System, Some(("a b", false))),
            ("0 3 * * * a b", User, Some(("a b", false))),
            ("@reboot root a", System, Some(("a", true))),
            ("@daily root a", System, Some(("a", false))),
            ("@reboot a", User, Some(("a", true))),
            ("@daily a", User, Some(("a", false))),
            ("1 5 job a b", Anacron, Some(("a b", false))),
            ("@monthly 15 job a", Anacron, Some(("a", false))),
            // cron's newline: an unescaped `%` ends the command; a
            // backslash escapes the next character, a backslash included.
            ("0 3 * * * a % input", User, Some(("a", false))),
            ("0 3 * * * a \\% b % c", User, Some(("a \\% b", false))),
            ("0 3 * * * a \\\\% b", User, Some(("a \\\\", false))),
            ("0 3 * * * a \\b% c", User, Some(("a \\b", false))),
            // No command: a comment, a blank line, a setting, too few fields.
            ("# 0 3 * * * a", User, None),
            ("   ", User, None),
            ("SHELL=/bin/sh", User, None),
            ("MAILTO = root", System, None),
            ("0 3 * * *", User, None),
            ("0 3 * * * root", System, None),
            ("0 3 * * * %x", User, None),
        ] {
            assert_eq!(cron_command(line, table), want, "{line:?}");
        }
    }

    #[test]
    fn a_cron_command_s_program_is_where_its_table_puts_it() {
        // Under a mount point of that OS, a program may be another file at
        // boot; were the user read as the program, it would be passed over.
        for (rel, line) in [
            ("etc/crontab", "0 3 * * * root /home/x/backup.sh\n"),
            ("etc/cron.d/backup", "@reboot root /home/x/backup.sh\n"),
            ("var/spool/cron/root", "0 3 * * * /home/x/backup.sh\n"),
            ("etc/anacrontab", "1 5 backup /home/x/backup.sh\n"),
        ] {
            let d = tempfile::tempdir().unwrap();
            let root = d.path();
            cron_root(root);
            write(root, "etc/fstab", "x /home btrfs subvol=@home 0 0\n");
            write(root, rel, line);
            assert_eq!(at_boot(root).verdict, BootVerdict::May, "{rel}");
        }
    }

    #[test]
    fn dot_and_source_are_the_shell_s_own_not_programs_looked_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configure(root);
        // A mounted /usr/local: `.` or `source` looked up there as a program
        // may be anything; as the shell's own there is nothing to look up.
        write(root, "etc/fstab", "x /usr/local btrfs subvol=@local 0 0\n");
        unit(
            root,
            ETC,
            "backup.service",
            "[Service]\nExecStart=/usr/bin/sh -c '. /etc/x.sh; source /etc/x.sh'\n",
        );
        enable(root, ETC, "multi-user.target.wants", "backup.service");
        script(root, "etc/x.sh", "/usr/bin/true\n");
        assert_eq!(at_boot(root), nothing());
        script(root, "etc/x.sh", "/usr/bin/btrbk -c /opt/x.conf run\n");
        assert_eq!(
            at_boot(root).runners[0].config.as_deref(),
            Some("/opt/x.conf")
        );
    }

    #[test]
    fn cron_enabled_with_a_btrbk_line_will_run_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cron_root(root);
        write(
            root,
            "etc/cron.d/backup",
            "# btrbk nightly\nSHELL=/bin/sh\n0 3 * * * root /usr/bin/btrbk -q run\n",
        );
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.reasons,
            [format!(
                "etc/cron.d/backup (cron, cronie.service) runs btrbk on its cron schedule, {PRESENT}"
            )]
        );
        assert_eq!(
            b.runners,
            [runner(
                "etc/cron.d/backup",
                Some("cronie.service"),
                "on its cron schedule",
                None,
                Some(true)
            )]
        );
        // Every place cron reads.
        fs::remove_file(root.join("etc/cron.d/backup")).unwrap();
        write(root, "etc/crontab", "15 1 * * * root btrbk run\n");
        write(root, "etc/anacrontab", "1 5 btrbk-daily btrbk run\n");
        write(
            root,
            "etc/cron.daily/btrbk",
            "#!/bin/sh\nexec /usr/bin/btrbk -q run\n",
        );
        write(
            root,
            "var/spool/cron/root",
            "30 2 * * * btrbk -c /opt/x.conf run\n",
        );
        write(
            root,
            "var/spool/cron/crontabs/root",
            "30 2 * * * /usr/bin/btrbk run\n",
        );
        let sources: Vec<String> = at_boot(root)
            .runners
            .into_iter()
            .map(|r| r.source)
            .collect();
        assert_eq!(
            sources,
            [
                "etc/crontab",
                "etc/anacrontab",
                "etc/cron.daily/btrbk",
                "var/spool/cron/root",
                "var/spool/cron/crontabs/root"
            ]
        );
    }

    #[test]
    fn cron_enabled_with_a_table_that_cannot_be_read_may_run_btrbk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cron_root(root);
        let crontab = root.join("etc/crontab");
        link(root, "etc/crontab", "/etc/crontab");
        let b = read_refusing(root).at_boot;
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [format!(
                "cron (cronie.service) is enabled and {}",
                unread("etc/crontab")
            )]
        );
        // Read, it leads round to itself.
        assert_eq!(
            at_boot(root).reasons,
            ["cron (cronie.service) is enabled and etc/crontab: more than 8 links"]
        );
        // Too large to read whole: not read in part.
        fs::remove_file(&crontab).unwrap();
        fs::write(&crontab, "#".repeat(64 * 1024 + 1)).unwrap();
        assert_eq!(
            at_boot(root).reasons,
            ["cron (cronie.service) is enabled and etc/crontab: larger than 64 KiB, not read"]
        );
        fs::write(&crontab, "#".repeat(64 * 1024)).unwrap();
        assert_eq!(at_boot(root), nothing(), "64 KiB itself is read");
        // Unreadable by permission, as for any user (root is made one).
        fs::write(&crontab, "0 3 * * * root btrbk run\n").unwrap();
        fs::set_permissions(&crontab, fs::Permissions::from_mode(0o000)).unwrap();
        let b = testutil::as_unprivileged(Some(root), || at_boot(root));
        fs::set_permissions(&crontab, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            ["cron (cronie.service) is enabled and etc/crontab: Permission denied (os error 13)"]
        );
        // A table directory that is a link: unknown too.
        fs::remove_file(&crontab).unwrap();
        link(root, "etc/cron.d", "/etc/cron.d");
        assert_eq!(at_boot(root).verdict, BootVerdict::May);
    }

    #[test]
    fn cron_that_is_not_enabled_runs_nothing_whatever_its_tables_say() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        unit(
            root,
            VENDOR,
            "cronie.service",
            "[Service]\nExecStart=/usr/bin/crond -n\n",
        );
        write(root, "etc/crontab", "0 3 * * * root /usr/bin/btrbk run\n");
        configure(root);
        assert_eq!(at_boot(root), nothing(), "installed, not enabled");
        enable(root, ETC, "multi-user.target.wants", "cronie.service");
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
        // anacron's timer enables it the same way.
        fs::remove_dir_all(root.join(ETC)).unwrap();
        enable(root, VENDOR, "timers.target.wants", "anacron.timer");
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
    }

    // ---- the readings themselves -------------------------------------------

    #[test]
    fn the_btrbk_config_is_the_first_of_the_two_btrbk_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let first = "etc/btrbk.conf";
        let second = "etc/btrbk/btrbk.conf";
        assert_eq!(
            read_btrbk_config(root, &UNMOUNTED, FOLLOW),
            BtrbkConfig::Absent
        );
        write(root, second, CONF);
        let at_second = BtrbkConfig::Present {
            path: "/etc/btrbk/btrbk.conf".into(),
            size_bytes: CONF.len() as u64,
        };
        assert_eq!(read_btrbk_config(root, &UNMOUNTED, FOLLOW), at_second);
        // btrbk 0.32 takes /etc/btrbk.conf when it exists, whatever else does.
        write(root, first, "volume /a\n");
        let at_first = BtrbkConfig::Present {
            path: "/etc/btrbk.conf".into(),
            size_bytes: 10,
        };
        assert_eq!(read_btrbk_config(root, &UNMOUNTED, FOLLOW), at_first);
        fs::remove_file(root.join(second)).unwrap();
        assert_eq!(read_btrbk_config(root, &UNMOUNTED, FOLLOW), at_first);
        write(root, first, "");
        assert_eq!(
            read_btrbk_config(root, &UNMOUNTED, FOLLOW),
            BtrbkConfig::Present {
                path: "/etc/btrbk.conf".into(),
                size_bytes: 0
            },
            "empty is still a config btrbk takes"
        );
        // Something else where the first should be: btrbk would take it, so
        // unknown — and the second is not consulted.
        fs::remove_file(root.join(first)).unwrap();
        fs::create_dir(root.join(first)).unwrap();
        write(root, second, CONF);
        let not_file = BtrbkConfig::Unreadable {
            reason: "etc/btrbk.conf: not a regular file".into(),
        };
        assert_eq!(read_btrbk_config(root, &UNMOUNTED, FOLLOW), not_file);
        assert!(
            read(root)
                .problems
                .contains(&"etc/btrbk.conf: not a regular file".to_string())
        );
        fs::remove_dir(root.join(first)).unwrap();
        fs::remove_file(root.join(second)).unwrap();
        fs::create_dir(root.join(second)).unwrap();
        assert_eq!(
            read_btrbk_config(root, &UNMOUNTED, FOLLOW),
            BtrbkConfig::Unreadable {
                reason: "etc/btrbk/btrbk.conf: not a regular file".into()
            }
        );
        // A link not read: unknown. Read: where it leads.
        fs::remove_dir(root.join(second)).unwrap();
        link(root, second, "/etc/hostname");
        assert_eq!(
            read_btrbk_config(root, &UNMOUNTED, REFUSE),
            BtrbkConfig::Unreadable {
                reason: unread(second)
            }
        );
        assert_eq!(
            read_btrbk_config(root, &UNMOUNTED, FOLLOW),
            BtrbkConfig::Absent
        );
        write(root, "etc/hostname", "x\n");
        assert_eq!(
            read_btrbk_config(root, &UNMOUNTED, FOLLOW),
            BtrbkConfig::Present {
                path: "/etc/btrbk/btrbk.conf".into(),
                size_bytes: 2
            }
        );
        fs::remove_file(root.join(second)).unwrap();
        link(root, second, "/dev/null");
        assert_eq!(
            read_btrbk_config(root, &UNMOUNTED, FOLLOW),
            BtrbkConfig::Unreadable {
                reason: "etc/btrbk/btrbk.conf: a link to /dev/null, which btrbk would read".into()
            }
        );
    }

    #[test]
    fn a_unit_tree_or_dependency_directory_that_is_a_link_makes_the_units_unknown() {
        let outside = tempfile::tempdir().unwrap();
        enable(outside.path(), ETC, "timers.target.wants", "btrbk.timer");
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(ETC)).unwrap();
        let wants = root.join(ETC).join("timers.target.wants");
        symlink(outside.path().join(ETC).join("timers.target.wants"), &wants).unwrap();
        configure(root);
        let why = format!(
            "etc/systemd/system/timers.target.wants: {} is a symlink, not followed",
            wants.display()
        );
        let r = read(root);
        assert_eq!(
            r.units,
            EnabledUnits::Unreadable {
                reason: why.clone()
            }
        );
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
        assert_eq!(r.at_boot.reasons, [format!("enabled units unknown: {why}")]);
        assert_eq!(r.problems, [why]);
        // The tree itself a link, or a file.
        let linked = tempfile::tempdir().unwrap();
        fs::create_dir_all(linked.path().join("etc/systemd")).unwrap();
        symlink(outside.path().join(ETC), linked.path().join(ETC)).unwrap();
        assert!(
            matches!(read(linked.path()).units, EnabledUnits::Unreadable { reason }
            if reason.ends_with("is a symlink, not followed"))
        );
        let file = tempfile::tempdir().unwrap();
        write(file.path(), ETC, "a file");
        assert!(
            matches!(read(file.path()).units, EnabledUnits::Unreadable { reason }
            if reason.starts_with("etc/systemd/system: "))
        );
    }

    #[test]
    fn a_dependency_directory_that_cannot_be_listed_is_unknown_never_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        enable(root, ETC, "timers.target.wants", "btrbk.timer");
        configure(root);
        let wants = root.join(ETC).join("timers.target.wants");
        fs::set_permissions(&wants, fs::Permissions::from_mode(0o000)).unwrap();
        let r = testutil::as_unprivileged(Some(root), || read(root));
        fs::set_permissions(&wants, fs::Permissions::from_mode(0o755)).unwrap();
        let why = "etc/systemd/system/timers.target.wants: Permission denied (os error 13)";
        assert_eq!(r.units, EnabledUnits::Unreadable { reason: why.into() });
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
        assert_eq!(r.problems, [why]);
    }

    #[test]
    fn what_an_entry_is_comes_from_its_own_lstat_and_odd_names_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A FIFO with a dependency directory's name enables nothing, and is
        // never opened (opening a FIFO blocks).
        // A writer stands by, so a regression that opens it fails instead of
        // hanging the test.
        let _fifo = Fifo::new(root, "etc/systemd/system/timers.target.wants");
        enable(root, ETC, "multi-user.target.wants", "sshd.service");
        assert_eq!(
            read(root).units,
            EnabledUnits::Listed {
                units: vec![EnabledUnit {
                    name: "sshd.service".into(),
                    dirs: vec!["etc/systemd/system/multi-user.target.wants".into()],
                }]
            }
        );
        for rel in ["etc/..", "..", "", "etc/systemd/system/."] {
            assert!(
                matches!(entry_at(root, rel), Entry::Unreadable(why)
                    if why == format!("{rel}: not a plain relative path")),
                "{rel:?}"
            );
        }
        // A -c of / names no file: unknown.
        package_btrbk(root);
        enable(root, ETC, "multi-user.target.wants", "btrbk.service");
        write(
            root,
            "etc/systemd/system/btrbk.service.d/c.conf",
            "[Service]\nExecStart=\nExecStart=/usr/bin/btrbk -c / run\n",
        );
        assert_eq!(at_boot(root).verdict, BootVerdict::May);
    }

    #[test]
    fn a_timer_starting_a_missing_unit_named_for_btrbk_may_run_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configure(root);
        unit(
            root,
            ETC,
            "nightly.timer",
            "[Timer]\nOnCalendar=daily\nUnit=btrbk-gone.service\n",
        );
        enable(root, ETC, "timers.target.wants", "nightly.timer");
        assert_eq!(
            at_boot(root).reasons,
            [
                "nightly.timer starts btrbk-gone.service, which is named for btrbk, and its unit \
                 file was not found"
            ]
        );
        // Neither name is btrbk's: a missing unit cannot start, so no.
        unit(
            root,
            ETC,
            "nightly.timer",
            "[Timer]\nOnCalendar=daily\nUnit=gone.service\n",
        );
        assert_eq!(at_boot(root), nothing());
    }

    #[test]
    fn the_same_runner_found_twice_is_recorded_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configure(root);
        unit(
            root,
            ETC,
            "twice.service",
            "[Service]\nExecStart=/bin/sh -c 'btrbk run; btrbk run'\n\
             ExecStartPost=/usr/bin/btrbk run\n",
        );
        enable(root, ETC, "multi-user.target.wants", "twice.service");
        let b = at_boot(root);
        assert_eq!(
            b.runners,
            [runner(
                "twice.service",
                None,
                "at every boot",
                None,
                Some(true)
            )]
        );
        assert_eq!(b.reasons.len(), 1, "{:?}", b.reasons);
    }

    #[test]
    fn a_link_among_the_drop_ins_not_read_is_skipped_for_the_vendor_and_unknown_for_etc() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        enable(root, ETC, "multi-user.target.wants", "btrbk.service");
        configure(root);
        // The package's own links among its drop-ins, file or directory.
        link(
            root,
            "usr/lib/systemd/system/btrbk.service.d/10-alias.conf",
            "/usr/lib/systemd/system/other.conf",
        );
        link(root, "usr/lib/systemd/system/service.d", "/usr/share/x");
        assert_eq!(
            read_refusing(root).at_boot.verdict,
            BootVerdict::Will,
            "skipped"
        );
        assert_eq!(
            at_boot(root).verdict,
            BootVerdict::Will,
            "read: lead nowhere"
        );
        // An administrator's link: a directory of drop-ins, unknown unread.
        link(root, "etc/systemd/system/btrbk.service.d", "/etc/elsewhere");
        let b = read_refusing(root).at_boot;
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [format!(
                "btrbk.service may run btrbk: {}",
                unread("etc/systemd/system/btrbk.service.d")
            )]
        );
        // Read: what it leads to is read — a reset there runs nothing.
        assert_eq!(at_boot(root).verdict, BootVerdict::Will, "leads nowhere");
        write(root, "etc/elsewhere/reset.conf", "[Service]\nExecStart=\n");
        assert_eq!(at_boot(root), nothing());
        // A drop-in linked into place is read; one linked to /dev/null is empty.
        fs::remove_file(root.join("etc/elsewhere/reset.conf")).unwrap();
        write(
            root,
            "etc/x/run.conf",
            "[Service]\nExecStartPost=/usr/bin/btrbk -c /opt/n.conf run\n",
        );
        link(root, "etc/elsewhere/run.conf", "/etc/x/run.conf");
        assert_eq!(at_boot(root).runners.len(), 2);
        fs::remove_file(root.join("etc/elsewhere/run.conf")).unwrap();
        link(root, "etc/elsewhere/run.conf", "/dev/null");
        assert_eq!(at_boot(root).runners.len(), 1, "masked drop-in");
    }

    #[test]
    fn what_could_not_be_read_is_a_problem_once_however_often_it_is_met() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configure(root);
        for name in ["a.service", "b.service"] {
            unit(root, VENDOR, name, "[Service]\nExecStart=/usr/bin/true\n");
            enable(root, ETC, "multi-user.target.wants", name);
        }
        // One masked type-level drop-in, met by both services.
        link(
            root,
            "etc/systemd/system/service.d/50-mask.conf",
            "/dev/null",
        );
        let why = unread("etc/systemd/system/service.d/50-mask.conf");
        let r = read_refusing(root);
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
        assert_eq!(r.at_boot.reasons.len(), 2, "{:?}", r.at_boot.reasons);
        assert_eq!(r.problems, [why]);
    }

    // ---- words, commands and settings -----------------------------------

    #[test]
    fn btrbk_is_found_in_any_word_of_a_command_with_the_config_after_it() {
        use ConfigArg::{Default as D, Path as P, Unresolved as U};
        let d = || Found::Btrbk(D);
        let p = |s: &str| Found::Btrbk(P(s.to_string()));
        let u = |s: &str| Found::Btrbk(U(s.to_string()));
        let like = |s: &str| Found::Like(s.to_string());
        let cases: Vec<(&str, Vec<Found>)> = vec![
            ("/usr/bin/btrbk run", vec![d()]),
            ("btrbk -q run", vec![d()]),
            ("-/usr/bin/btrbk run", vec![d()]),
            ("!!/usr/bin/btrbk run", vec![d()]),
            // systemd's `|` prefix is split off as an operator.
            ("|/usr/bin/btrbk run", vec![d()]),
            ("|@/usr/bin/btrbk run", vec![d()]),
            ("/usr/bin/nice -n 19 /usr/bin/btrbk run", vec![d()]),
            ("/usr/bin/ionice -c3 btrbk run", vec![d()]),
            (
                "/bin/sh -c 'btrbk -c /opt/x.conf run'",
                vec![p("/opt/x.conf")],
            ),
            (
                "/bin/sh -c \"cd / && btrbk --config=/opt/x.conf run\"",
                vec![p("/opt/x.conf")],
            ),
            (
                "/bin/sh -c 'btrbk -c \"/opt/my x.conf\" run'",
                vec![p("/opt/my x.conf")],
            ),
            ("btrbk --config /opt/x.conf run", vec![p("/opt/x.conf")]),
            ("btrbk -c/opt/x.conf run", vec![p("/opt/x.conf")]),
            ("btrbk -qc /opt/x.conf run", vec![u("-qc")]),
            ("btrbk -c $CONF run", vec![u("$CONF")]),
            ("btrbk -c %h/x.conf run", vec![u("%h/x.conf")]),
            ("btrbk -c x.conf run", vec![u("x.conf")]),
            ("btrbk -c", vec![u("-c")]),
            ("btrbk -c ; x", vec![u("-c")]),
            ("btrbk run; foo -c /x", vec![d()]),
            ("btrbk run && bar -c /x", vec![d()]),
            ("btrbk run # -c /x", vec![d()]),
            ("btrbk run; btrbk -c /y run", vec![d(), p("/y")]),
            // What a backtick substitutes is an argument only that OS knows.
            ("btrbk -c /y run `opts`", vec![u("`")]),
            ("btrbk `opts`", vec![u("`")]),
            ("x `btrbk -c /y run`", vec![p("/y")]),
            // A quoted script that is btrbk alone still runs it.
            ("/bin/sh -c '\"btrbk\"'", vec![d()]),
            ("btrbk clean", vec![d()]),
            ("btrbk -1c run", vec![d()]),
            ("btrbk -c /opt/my\\ x.conf run", vec![p("/opt/my x.conf")]),
            ("btrbk -c \"/opt/a\\\"b\" run", vec![p("/opt/a\"b")]),
            ("btrbk -c \"\" run", vec![u("")]),
            // btrbk's own arguments are its own, not looked at again.
            (
                "btrbk -c /etc/btrbk/btrbk.conf run",
                vec![p("/etc/btrbk/btrbk.conf")],
            ),
            ("btrbk -c /etc/btrbk run", vec![p("/etc/btrbk")]),
            (
                "btrbk --config=/etc/btrbk.conf run",
                vec![p("/etc/btrbk.conf")],
            ),
            // Only like btrbk's name: what it runs is unknown.
            (
                "/usr/bin/btrbk-wrapper run",
                vec![like("/usr/bin/btrbk-wrapper")],
            ),
            (
                "/usr/local/bin/btrbk.sh",
                vec![like("/usr/local/bin/btrbk.sh")],
            ),
            ("sh -c 'run-btrbk --all'", vec![like("run-btrbk")]),
            (
                "/usr/bin/cat /etc/btrbk.conf",
                vec![like("/etc/btrbk.conf")],
            ),
            ("echo btrbkx xbtrbk", vec![like("btrbkx"), like("xbtrbk")]),
            (
                "btrbk run | logger -t btrbk-run",
                vec![d(), like("btrbk-run")],
            ),
            ("/usr/bin/cat /etc/fstab", vec![]),
            ("echo BTRBK_DONE", vec![]),
            ("", vec![]),
            // Cautious: a path ending in btrbk counts, even as an argument.
            ("tar czf /tmp/x.tgz /etc/btrbk", vec![d()]),
        ];
        for (line, want) in cases {
            assert_eq!(btrbk_invocations(&words(line)), want, "{line:?}");
        }
        for (word, want) in [
            ("btrbk", Naming::Btrbk),
            ("/usr/bin/btrbk", Naming::Btrbk),
            ("-@:+!btrbk", Naming::Btrbk),
            ("btrbk.sh", Naming::Like),
            ("/opt/x/run-btrbk", Naming::Like),
            ("/opt/btrbk/run", Naming::Not),
            ("Btrbk", Naming::Not),
            ("", Naming::Not),
        ] {
            assert_eq!(naming(word), want, "{word:?}");
        }
    }

    #[test]
    fn a_command_line_launches_its_programs_and_what_they_run() {
        use Grammar::{Shell, Systemd};
        let launched = |line: &str, grammar: Grammar| -> Vec<String> {
            launches(&words(line), grammar)
                .into_iter()
                .map(|l| match l {
                    Launch::Program {
                        word,
                        role,
                        literal,
                        ..
                    } => format!("{word} {role:?}{}", if literal { " literal" } else { "" }),
                    Launch::Inline(line) => format!("[{line}]"),
                })
                .collect()
        };
        let cases: Vec<(&str, Grammar, Vec<&str>)> = vec![
            // systemd's: the program after its prefixes, literally; `@`
            // gives it an argv[0]; a lone `;` starts another command; other
            // operators are arguments.
            (
                "-@/usr/bin/x argv0 /a",
                Systemd,
                vec!["/usr/bin/x Program literal"],
            ),
            (
                "/usr/bin/a ; /usr/bin/b x",
                Systemd,
                vec!["/usr/bin/a Program literal", "/usr/bin/b Program literal"],
            ),
            (
                "/usr/bin/a | /usr/bin/b",
                Systemd,
                vec!["/usr/bin/a Program literal"],
            ),
            // Shells: a -c line (a cluster with c too), else the script.
            (
                "/bin/sh -c 'cd /srv && /opt/r.sh'",
                Systemd,
                vec!["/bin/sh Program literal", "[cd /srv && /opt/r.sh]"],
            ),
            (
                "/bin/bash -ec x",
                Systemd,
                vec!["/bin/bash Program literal", "[x]"],
            ),
            (
                "/bin/sh -o pipefail /opt/s.sh arg",
                Systemd,
                vec!["/bin/sh Program literal", "/opt/s.sh Script"],
            ),
            (
                "bash --rcfile /etc/r /opt/s.sh",
                Shell,
                vec!["bash Program", "/opt/s.sh Script"],
            ),
            (". /etc/x.sh", Shell, vec![". Program", "/etc/x.sh Script"]),
            (
                "source /etc/x.sh",
                Shell,
                vec!["source Program", "/etc/x.sh Script"],
            ),
            // Interpreters: the script, unless code is given instead.
            (
                "python3 -u /opt/x.py",
                Shell,
                vec!["python3 Program", "/opt/x.py Script"],
            ),
            ("python3 -c 'import os'", Shell, vec!["python3 Program"]),
            (
                "perl -I /opt/lib /opt/x.pl",
                Shell,
                vec!["perl Program", "/opt/x.pl Script"],
            ),
            // Wrappers: past options, their values, operands, assignments.
            (
                "env -u X FOO=1 /usr/local/bin/b.sh x",
                Shell,
                vec!["env Program", "/usr/local/bin/b.sh Program"],
            ),
            (
                "env -S 'btrbk run'",
                Shell,
                vec!["env Program", "[btrbk run]"],
            ),
            ("env --split-string=x", Shell, vec!["env Program", "[x]"]),
            (
                "nice -n 19 ionice -c 3 /opt/b.sh",
                Shell,
                vec!["nice Program", "ionice Program", "/opt/b.sh Program"],
            ),
            (
                "flock -w 5 /run/lock/x /opt/b.sh",
                Shell,
                vec!["flock Program", "/opt/b.sh Program"],
            ),
            (
                "flock /run/lock/x -c /opt/b.sh",
                Shell,
                vec!["flock Program", "[/opt/b.sh]"],
            ),
            (
                "timeout -k 5 1h /opt/b.sh",
                Shell,
                vec!["timeout Program", "/opt/b.sh Program"],
            ),
            (
                "sudo -u root -- -/opt/b.sh",
                Shell,
                vec!["sudo Program", "-/opt/b.sh Program"],
            ),
            (
                "su -s /bin/sh root -c /opt/b.sh",
                Shell,
                vec!["su Program", "[/opt/b.sh]"],
            ),
            // sh's: past assignments and keywords, every command; a loop's
            // or a case's header runs nothing.
            (
                "FOO=1 BAR=2 /opt/a; if /opt/b; then exec /opt/c; fi",
                Shell,
                vec![
                    "/opt/a Program",
                    "/opt/b Program",
                    "exec Program",
                    "/opt/c Program",
                ],
            ),
            (
                "for x in /opt/a /opt/b; do /opt/c; done",
                Shell,
                vec!["/opt/c Program"],
            ),
            (
                "echo /opt/a | /opt/b && /opt/c",
                Shell,
                vec!["echo Program", "/opt/b Program", "/opt/c Program"],
            ),
            // A program this does not know runs nothing it can tell.
            (
                "/usr/bin/foo /opt/b.sh",
                Shell,
                vec!["/usr/bin/foo Program"],
            ),
            ("", Shell, vec![]),
        ];
        for (line, grammar, want) in cases {
            assert_eq!(launched(line, grammar), want, "{line:?}");
        }
        assert!(is_assignment("A_1=x") && is_assignment("_=") && !is_assignment("1A=x"));
        assert!(!is_assignment("a-b=x") && !is_assignment("=x") && !is_assignment("ab"));
    }

    #[test]
    fn a_command_resolves_its_unit_s_own_specifiers_and_keeps_the_rest() {
        let unit = "backup@daily.service";
        for (raw, want, resolved) in [
            (
                "/opt/%i.sh /opt/%p/%j.sh %n %N 100%%",
                "/opt/daily.sh /opt/backup/backup.sh backup@daily.service backup@daily 100%",
                true,
            ),
            (
                "btrbk -c /etc/btrbk/%i.conf run",
                "btrbk -c /etc/btrbk/daily.conf run",
                true,
            ),
            ("/opt/%H.sh %t/x", "/opt/%H.sh %t/x", false),
            ("trailing %", "trailing %", false),
            ("no specifier", "no specifier", true),
        ] {
            assert_eq!(
                expand_specifiers(unit, raw),
                (want.to_string(), resolved),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn a_name_resolves_its_own_specifiers_and_a_template_takes_an_instance() {
        for (unit, raw, want) in [
            (
                "job@daily.service",
                "snap@%i.service",
                Some("snap@daily.service"),
            ),
            (
                "job@daily.service",
                "snap@.service",
                Some("snap@daily.service"),
            ),
            ("job.service", "snap@%i.socket", Some("snap@job.socket")),
            // A template itself (an Accept=yes socket's service) has no
            // instance yet: each connection's is its own.
            ("job@.service", "snap@%i.service", None),
            ("job@.service", "snap@%n.service", None),
            ("job@.service", "snap@%N.service", None),
            ("job@.service", "snap@%p.service", Some("snap@job.service")),
            ("job.service", "snap@.socket", Some("snap@job.socket")),
            (
                "db-backup@x.service",
                "%p-%j-%N-%n",
                Some("db-backup-backup-db-backup@x-db-backup@x.service"),
            ),
            ("job.service", "%p %N %j %i.", Some("job job job .")),
            ("a.service", "100%%.target", Some("100%.target")),
            ("a.service", "%H.service", None),
            ("a.service", "trailing%", None),
            ("a.service", "plain.service", Some("plain.service")),
            // Only a template is instantiated: an instance, or a name with
            // two `@`, is left as it is.
            ("job@d.service", "x@y.service", Some("x@y.service")),
            ("job@d.service", "x@@.service", Some("x@@.service")),
            ("job@d.service", "noat", Some("noat")),
        ] {
            assert_eq!(resolve_name(unit, raw).as_deref(), want, "{unit} {raw}");
        }
    }

    /// `name`'s own file, as the walk finds it, links read.
    fn unit_file(root: &Path, name: &str) -> UnitFile {
        let trees: Vec<Tree> = UNIT_TREES
            .iter()
            .map(|rel| Tree {
                rel,
                listing: list_dir(root, rel),
            })
            .collect();
        find_unit_file(root, &trees, name, FOLLOW)
    }

    #[test]
    fn a_link_to_a_template_aliases_the_same_instance_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let foo = "[Service]\nExecStart=/usr/bin/foo %i\n";
        write(root, &format!("{VENDOR}/foo@.service"), foo);
        write(root, &format!("{VENDOR}/bar@.service"), "[Service]\n");
        write(root, &format!("{VENDOR}/b.service"), "[Service]\n");
        // A template aliased by a template: every instance of it.
        link(
            root,
            &format!("{ETC}/alias@.service"),
            &format!("/{VENDOR}/bar@.service"),
        );
        assert_eq!(
            unit_file(root, "alias@x.service"),
            UnitFile::Alias("bar@x.service".into())
        );
        assert_eq!(
            unit_file(root, "alias@.service"),
            UnitFile::Alias("bar@.service".into())
        );
        // Just one instance linked to a different template: that instance.
        link(
            root,
            &format!("{ETC}/one@y.service"),
            &format!("/{VENDOR}/bar@.service"),
        );
        assert_eq!(
            unit_file(root, "one@y.service"),
            UnitFile::Alias("bar@y.service".into())
        );
        // An instance linked to its own template: that is its file.
        link(
            root,
            &format!("{ETC}/foo@z.service"),
            &format!("/{VENDOR}/foo@.service"),
        );
        assert_eq!(unit_file(root, "foo@z.service"), UnitFile::Text(foo.into()));
        // A plain name linked to another.
        link(
            root,
            &format!("{ETC}/a.service"),
            &format!("/{VENDOR}/b.service"),
        );
        assert_eq!(
            unit_file(root, "a.service"),
            UnitFile::Alias("b.service".into())
        );
    }

    #[test]
    fn a_unit_file_over_64_kib_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let head = "[Service]\n";
        let at_limit = format!("{head}{}", "#".repeat(64 * 1024 - head.len()));
        write(root, &format!("{VENDOR}/big.service"), &at_limit);
        assert_eq!(
            unit_file(root, "big.service"),
            UnitFile::Text(at_limit.clone())
        );
        write(
            root,
            &format!("{VENDOR}/big.service"),
            &format!("{at_limit}#"),
        );
        assert_eq!(
            unit_file(root, "big.service"),
            UnitFile::Unknown(format!(
                "{VENDOR}/big.service: larger than 64 KiB, not read"
            ))
        );
    }

    #[test]
    fn unit_settings_follow_sections_continuations_comments_and_resets() {
        let s = parse_unit([
            "ExecStart=/usr/bin/outside-any-section\n\
             [Unit]\nExecStart=/usr/bin/btrbk run\n\
             [Service]\nType=oneshot\n# ExecStart=/usr/bin/btrbk\n; ExecStart=/usr/bin/btrbk\n\
             ExecStart=/usr/bin/a \\\n# a comment inside the continuation\n  --long\n\
             ExecStartPre=-/usr/bin/b\nExecStart=\nExecStart = /usr/bin/c\n"
                .to_string(),
            "[Service]\nExecStop=/usr/bin/d\n[Timer]\nUnit=x.service\nPersistent=Yes\n\
             OnCalendar=daily\nOnBootSec=5min\n"
                .to_string(),
            "ExecStart=/usr/bin/no-section-in-a-drop-in\n".to_string(),
        ]);
        let exec: Vec<(&str, Vec<&str>)> = s
            .exec
            .iter()
            .map(|(k, v)| (k.as_str(), v.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            exec,
            [
                ("ExecStart", vec!["/usr/bin/c"]),
                ("ExecStartPre", vec!["-/usr/bin/b"]),
                ("ExecStop", vec!["/usr/bin/d"]),
            ]
        );
        assert_eq!(s.unit.as_deref(), Some("x.service"));
        assert!(s.persistent && s.on_calendar && s.on_boot);
        // A continuation joins with a space, and a reset clears only its key.
        let s = parse_unit([
            "[Service]\nExecStart=/usr/bin/a \\\n --x\nExecStartPost=/b\nExecStartPost=\n"
                .to_string(),
        ]);
        assert_eq!(s.exec["ExecStart"], ["/usr/bin/a   --x"]);
        assert!(!s.exec.contains_key("ExecStartPost"));
        // A comment line inside a continuation is dropped, not joined.
        let s = parse_unit([
            "[Service]\nExecStart=/usr/bin/a \\\n# no\n; nor this\n --b\n".to_string(),
        ]);
        assert_eq!(s.exec["ExecStart"], ["/usr/bin/a   --b"]);
        // A comment never continues, even ending in a backslash: the next
        // line stands on its own, as systemd reads it.
        let s = parse_unit(["[Service]\n# note \\\nExecStart=/usr/bin/btrbk run\n".to_string()]);
        assert_eq!(s.exec["ExecStart"], ["/usr/bin/btrbk run"]);
        let s = parse_unit(["[Service]\n; note \\\nExecStart=/usr/bin/btrbk run\n".to_string()]);
        assert_eq!(s.exec["ExecStart"], ["/usr/bin/btrbk run"]);
        // Two trailing backslashes are one escaped backslash: no continuation;
        // three are an escaped one and a continuation.
        let s = parse_unit(["[Service]\nExecStart=/a \\\\\nExecStartPost=/b\n".to_string()]);
        assert_eq!(s.exec["ExecStart"], ["/a \\\\"]);
        assert_eq!(s.exec["ExecStartPost"], ["/b"]);
        let s = parse_unit(["[Service]\nExecStart=/a \\\\\\\n --c\n".to_string()]);
        assert_eq!(s.exec["ExecStart"], ["/a \\\\  --c"]);
        // Empty assignments reset the timer settings too — any of them every
        // timer, calendar and monotonic alike (systemd.timer(5)).
        let s = parse_unit([
            "[Timer]\nUnit=a.service\nUnit=\nOnCalendar=daily\nOnCalendar=\nOnBootSec=1\nOnBootSec=\n"
                .to_string(),
        ]);
        assert_eq!(s, UnitSettings::default());
        for reset in [
            "OnActiveSec",
            "OnBootSec",
            "OnStartupSec",
            "OnUnitActiveSec",
            "OnUnitInactiveSec",
            "OnCalendar",
        ] {
            let s = parse_unit([format!(
                "[Timer]\nOnCalendar=daily\nOnBootSec=1\n{reset}=\nPersistent=true\n"
            )]);
            assert!(!s.on_calendar && !s.on_boot && s.persistent, "{reset}");
        }
        for (key, on_boot, on_calendar) in [
            ("OnActiveSec", true, false),
            ("OnBootSec", true, false),
            ("OnStartupSec", true, false),
            ("OnUnitActiveSec", false, false),
            ("OnUnitInactiveSec", false, false),
            ("OnCalendar", false, true),
        ] {
            let s = parse_unit([format!("[Timer]\n{key}=5min\n")]);
            assert_eq!((s.on_boot, s.on_calendar), (on_boot, on_calendar), "{key}");
        }
        // An unparsable boolean is ignored, so the last valid one stands.
        let s = parse_unit([
            "[Timer]\nPersistent=yes\nPersistent=\nPersistent=sometimes\n\
             [Socket]\nAccept=yes\nAccept=\n"
                .to_string(),
        ]);
        assert!(s.persistent && s.accept);
        let s = parse_unit([
            "[Timer]\nPersistent=yes\nPersistent=off\n[Socket]\nAccept=1\nAccept=0\n".to_string(),
        ]);
        assert!(!s.persistent && !s.accept);
        // What a socket starts: the last Service= that names a service.
        let s = parse_unit([
            "[Socket]\nService=a.service\nService=b.timer\nService=\n".to_string(),
            "[Service]\nService=c.service\n[Socket]\nService=d.service\n".to_string(),
        ]);
        assert_eq!(s.service.as_deref(), Some("d.service"));
        // What [Unit] pulls in, every name in order, never reset by an empty
        // assignment; elsewhere the same keys pull in nothing.
        let s = parse_unit([
            "[Unit]\nWants=a.service b.target\nRequires=c.service\nRequisite=d.service\n\
             BindsTo=e.service\nUpholds=f.service\nOnSuccess=g.service\nOnFailure=h.service\n\
             After=x.service\nPartOf=y.service\nConflicts=z.service\n"
                .to_string(),
            "[Unit]\nWants=\nOnFailure=\n[Install]\nWants=i.service\n[Service]\nRequires=j.service\n"
                .to_string(),
        ]);
        let pulls: Vec<(&str, &str)> = s
            .pulls
            .iter()
            .map(|(k, n)| (k.as_str(), n.as_str()))
            .collect();
        assert_eq!(
            pulls,
            [
                ("Wants", "a.service"),
                ("Wants", "b.target"),
                ("Requires", "c.service"),
                ("Requisite", "d.service"),
                ("BindsTo", "e.service"),
                ("Upholds", "f.service"),
                ("OnSuccess", "g.service"),
                ("OnFailure", "h.service"),
            ]
        );
        for (word, want) in [
            ("1", Some(true)),
            ("yes", Some(true)),
            ("Y", Some(true)),
            ("true", Some(true)),
            ("t", Some(true)),
            ("ON", Some(true)),
            ("0", Some(false)),
            ("no", Some(false)),
            ("N", Some(false)),
            ("false", Some(false)),
            ("f", Some(false)),
            ("off", Some(false)),
            ("", None),
            ("maybe", None),
        ] {
            assert_eq!(parse_boolean(word), want, "{word:?}");
        }
    }

    #[test]
    fn when_says_every_boot_unless_only_a_stopping_target_pulls_it_in() {
        let dirs = |d: &[&str]| d.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            boot_when(&dirs(&["etc/systemd/system/multi-user.target.wants"])),
            When::EveryBoot
        );
        assert_eq!(
            boot_when(&dirs(&["usr/lib/systemd/system/initrd.target.requires"])).text(),
            "when initrd.target starts"
        );
        assert_eq!(
            boot_when(&dirs(&[
                "etc/systemd/system/suspend.target.wants",
                "etc/systemd/system/sysinit.target.upholds"
            ])),
            When::EveryBoot
        );
        assert_eq!(boot_when(&[]), When::EveryBoot);
        // An unknown stamp says both.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("var/lib/systemd/timers/stamp-x.timer")).unwrap();
        let s = UnitSettings {
            persistent: true,
            on_calendar: true,
            ..UnitSettings::default()
        };
        assert_eq!(
            timer_when(dir.path(), "x.timer", &s).text(),
            "straight after boot or at its next scheduled time (its stamp is unknown)"
        );
        // Persistent alone, without OnCalendar=, catches nothing up.
        write(dir.path(), "var/lib/systemd/timers/stamp-y.timer", "");
        let s = UnitSettings {
            persistent: true,
            ..UnitSettings::default()
        };
        assert_eq!(
            timer_when(dir.path(), "y.timer", &s).text(),
            "at its next scheduled time after boot"
        );
        // How soon each is: a unit reached sooner passes that on.
        let ranked = [
            When::EveryBoot,
            When::StraightAfterBoot,
            When::SoonAfterBoot,
            When::CatchUpOrNext,
            When::NextScheduled,
            When::Connects,
            When::PathMet,
            When::Fails("a".into()),
            When::Succeeds("a".into()),
            When::Starts("shutdown.target".into()),
        ]
        .map(|w| w.rank());
        assert_eq!(ranked, [0, 1, 1, 2, 3, 4, 4, 4, 4, 5]);
    }

    // ---- what a socket starts ----------------------------------------------

    #[test]
    fn an_enabled_socket_starts_its_service_when_something_connects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configure(root);
        let listen = "[Socket]\nListenStream=/run/backup.sock\n";
        unit(root, VENDOR, "backup.socket", listen);
        unit(
            root,
            VENDOR,
            "backup.service",
            "[Service]\nExecStart=/usr/bin/btrbk run\n",
        );
        enable(root, ETC, "sockets.target.wants", "backup.socket");
        let when = "when something connects to it";
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.runners,
            [runner(
                "backup.service",
                Some("backup.socket"),
                when,
                None,
                Some(true)
            )]
        );
        assert_eq!(
            b.reasons,
            [format!(
                "backup.socket starts backup.service, which runs btrbk {when}, {PRESENT}"
            )]
        );
        // Service= names the one it starts; the same-named one is not read.
        unit(
            root,
            VENDOR,
            "backup.socket",
            &format!("{listen}Service=other.service\n"),
        );
        assert_eq!(at_boot(root), nothing());
        unit(
            root,
            VENDOR,
            "other.service",
            "[Service]\nExecStart=/bin/sh -c 'btrbk run'\n",
        );
        assert_eq!(
            at_boot(root).runners,
            [runner(
                "other.service",
                Some("backup.socket"),
                when,
                None,
                Some(true)
            )]
        );
        // An empty Service=, or one naming no service, is ignored as systemd
        // ignores it; a drop-in's Service= is the last word.
        let drop_in = "etc/systemd/system/backup.socket.d/s.conf";
        write(root, drop_in, "[Socket]\nService=\nService=x.timer\n");
        assert_eq!(at_boot(root).runners[0].source, "other.service");
        write(root, drop_in, "[Socket]\nService=backup.service\n");
        assert_eq!(at_boot(root).runners[0].source, "backup.service");
        // Accept=yes: an instance of its template per connection, nothing else.
        fs::remove_dir_all(root.join("etc/systemd/system/backup.socket.d")).unwrap();
        unit(
            root,
            VENDOR,
            "backup.socket",
            &format!("{listen}Accept=yes\n"),
        );
        assert_eq!(at_boot(root), nothing(), "backup.service is not started");
        unit(
            root,
            VENDOR,
            "backup@.service",
            "[Service]\nExecStart=-/usr/bin/btrbk run\n",
        );
        assert_eq!(
            at_boot(root).runners,
            [runner(
                "backup@.service",
                Some("backup.socket"),
                when,
                None,
                Some(true)
            )]
        );
        // An empty or unparsable Accept= is ignored (systemd 262: "Failed to
        // parse Accept=, ignoring"), so it stays yes; a valid one sets it.
        unit(
            root,
            VENDOR,
            "backup.socket",
            &format!("{listen}Accept=yes\nAccept=\nAccept=maybe\n"),
        );
        assert_eq!(at_boot(root).runners[0].source, "backup@.service");
        unit(
            root,
            VENDOR,
            "backup.socket",
            &format!("{listen}Accept=yes\nAccept=no\n"),
        );
        assert_eq!(at_boot(root).runners[0].source, "backup.service");
        // The socket's own commands run when it starts: at boot.
        unit(
            root,
            VENDOR,
            "backup.service",
            "[Service]\nExecStart=/usr/bin/true\n",
        );
        unit(
            root,
            VENDOR,
            "backup.socket",
            &format!("{listen}ExecStartPre=/usr/bin/btrbk run\n"),
        );
        assert_eq!(
            at_boot(root).runners,
            [runner(
                "backup.socket",
                None,
                "at every boot",
                None,
                Some(true)
            )]
        );
    }

    // ---- scripts a command runs ----------------------------------------------

    /// das-backup's own shape: a service whose command is a script that runs
    /// btrbk (das-backup.service → backup-run.sh → btrbk).
    fn wrapped(root: &Path, command: &str) {
        unit(
            root,
            ETC,
            "backup.service",
            &format!("[Service]\nType=oneshot\nExecStart={command}\n"),
        );
    }

    fn wrapper_root(root: &Path, command: &str) {
        configure(root);
        wrapped(root, command);
        enable(root, ETC, "multi-user.target.wants", "backup.service");
    }

    #[test]
    fn a_script_a_unit_runs_is_read_and_the_scripts_it_runs_four_deep() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        wrapper_root(root, "/usr/local/bin/backup.sh --daily");
        script(
            root,
            "usr/local/bin/backup.sh",
            "set -eu\n# btrbk is called below\nexec btrbk -q run\n",
        );
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.runners,
            [through(
                runner("backup.service", None, "at every boot", None, Some(true)),
                "/usr/local/bin/backup.sh"
            )]
        );
        assert_eq!(
            b.reasons,
            [format!(
                "backup.service runs btrbk through /usr/local/bin/backup.sh at every boot, \
                 {PRESENT}"
            )]
        );
        // Its -c is the config checked.
        script(
            root,
            "usr/local/bin/backup.sh",
            "btrbk -c /opt/b.conf run\n",
        );
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::No);
        assert_eq!(b.runners[0].config.as_deref(), Some("/opt/b.conf"));
        // A script that runs no btrbk: no.
        script(root, "usr/local/bin/backup.sh", "rsync -a /etc /backup\n");
        assert_eq!(at_boot(root), nothing());
        // A script the script runs is read too, by its absolute path or its
        // bare name, at most four scripts deep.
        script(root, "usr/local/bin/backup.sh", "/usr/local/bin/inner.sh\n");
        script(root, "usr/local/bin/inner.sh", "btrbk run\n");
        assert_eq!(
            at_boot(root).runners[0].script.as_deref(),
            Some("/usr/local/bin/inner.sh")
        );
        script(root, "usr/local/bin/backup.sh", "s2\n");
        script(root, "usr/bin/s2", "s3 \"$@\"\n");
        script(root, "usr/local/bin/s3", "exec /usr/bin/s4\n");
        script(root, "usr/bin/s4", "btrbk run\n");
        assert_eq!(at_boot(root).verdict, BootVerdict::Will, "four deep");
        script(root, "usr/bin/s4", "s5\n");
        script(root, "usr/bin/s5", "btrbk run\n");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            ["backup.service may run btrbk through /usr/bin/s5: a script 4 scripts deep, not read"]
        );
        // One that runs itself ends.
        script(root, "usr/bin/s2", "s2\nbtrbk run\n");
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
        script(root, "usr/local/bin/inner.sh", "btrbk run\n");
        // Any absolute path in the command, inside a quoted script too.
        script(root, "usr/local/bin/backup.sh", "btrbk run\n");
        wrapped(
            root,
            "/usr/bin/nice -n 19 /bin/sh -c 'cd / && /usr/local/bin/backup.sh --daily'",
        );
        assert_eq!(
            at_boot(root).runners[0].script.as_deref(),
            Some("/usr/local/bin/backup.sh")
        );
        // systemd's program is taken literally ("may not be a variable"): a
        // `$` or a glob in it is part of its name. A specifier it cannot
        // resolve here leaves it unknown.
        let odd = "usr/local/bin/$X.sh";
        script(root, odd, "btrbk run\n");
        wrapped(root, "/usr/local/bin/$X.sh");
        assert_eq!(
            at_boot(root).runners[0].script.as_deref(),
            Some("/usr/local/bin/$X.sh")
        );
        wrapped(root, "/usr/local/bin/*.sh");
        assert_eq!(at_boot(root), nothing(), "no file is named *.sh");
        wrapped(root, "/usr/local/bin/%H.sh");
        assert_eq!(
            at_boot(root).reasons,
            [
                "backup.service may run btrbk: /usr/local/bin/%H.sh names a specifier only \
              that OS's systemd can resolve"
            ]
        );
        // In a script sh expands them: only that OS could (declared).
        wrapped(root, "/usr/local/bin/backup.sh");
        script(root, "usr/local/bin/backup.sh", "/usr/local/bin/$X.sh\n");
        assert_eq!(at_boot(root), nothing(), "sh's $X");
        script(root, "usr/local/bin/backup.sh", "btrbk run\n");
        // The unit's own specifiers are its name's.
        wrapped(root, "/usr/local/bin/%p.sh");
        assert_eq!(
            at_boot(root).runners[0].script.as_deref(),
            Some("/usr/local/bin/backup.sh")
        );
    }

    #[test]
    fn a_script_a_cron_line_runs_is_read_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cron_root(root);
        write(
            root,
            "etc/cron.d/backup",
            "0 3 * * * root /usr/local/bin/backup.sh > /dev/null\n",
        );
        script(root, "usr/local/bin/backup.sh", "btrbk run\n");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.runners,
            [through(
                runner(
                    "etc/cron.d/backup",
                    Some("cronie.service"),
                    "on its cron schedule",
                    None,
                    Some(true)
                ),
                "/usr/local/bin/backup.sh"
            )]
        );
        assert_eq!(
            b.reasons,
            [format!(
                "etc/cron.d/backup (cron, cronie.service) runs btrbk through \
                 /usr/local/bin/backup.sh on its cron schedule, {PRESENT}"
            )]
        );
        // A cron script's commands are read the same way.
        fs::remove_file(root.join("etc/cron.d/backup")).unwrap();
        script(root, "etc/cron.hourly/backup", "/usr/local/bin/backup.sh\n");
        let b = at_boot(root);
        assert_eq!(b.runners.len(), 1, "{:?}", b.runners);
        assert_eq!(b.runners[0].source, "etc/cron.hourly/backup");
        assert_eq!(
            b.runners[0].script.as_deref(),
            Some("/usr/local/bin/backup.sh")
        );
    }

    #[test]
    fn a_script_over_the_cap_or_unreadable_may_run_btrbk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        wrapper_root(root, "/usr/local/bin/backup.sh");
        let path = root.join("usr/local/bin/backup.sh");
        // `len` bytes: a #! line, a comment filling the rest, and btrbk as the
        // very last bytes, with no newline after them.
        let sized = |len: usize| {
            let (head, tail) = ("#!/bin/sh\n", "\nbtrbk");
            format!("{head}{}{tail}", "#".repeat(len - head.len() - tail.len()))
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, sized(64 * 1024)).unwrap();
        assert_eq!(
            at_boot(root).verdict,
            BootVerdict::Will,
            "64 KiB is read whole, to its last byte"
        );
        fs::write(&path, sized(64 * 1024 + 1)).unwrap();
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [
                "backup.service may run btrbk through /usr/local/bin/backup.sh: \
                 usr/local/bin/backup.sh: larger than 64 KiB, not read"
            ]
        );
        // Unreadable, as for any user (root is made one).
        fs::write(&path, "#!/bin/sh\nbtrbk run\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let r = testutil::as_unprivileged(Some(root), || read(root));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let why = "usr/local/bin/backup.sh: Permission denied (os error 13)";
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
        assert_eq!(
            r.at_boot.reasons,
            [format!(
                "backup.service may run btrbk through /usr/local/bin/backup.sh: {why}"
            )]
        );
        assert_eq!(r.problems, [why]);
        // A FIFO in its place is never opened.
        fs::remove_file(&path).unwrap();
        let _fifo = Fifo::new(root, "usr/local/bin/backup.sh");
        assert_eq!(at_boot(root), nothing());
    }

    #[test]
    fn a_program_that_is_not_a_script_is_not_looked_into() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        wrapper_root(root, "/usr/local/bin/backup /usr/local/bin/backup.sh");
        // A binary, with btrbk's name among its bytes.
        write(
            root,
            "usr/local/bin/backup",
            "\x7fELF\x02\x01\x01\0btrbk run\n",
        );
        // A text file with no #! line.
        write(root, "usr/local/bin/backup.sh", "btrbk run\n");
        assert_eq!(at_boot(root), nothing(), "not looked into (declared)");
        // A script named through a link: not read, a program link is not
        // looked into (declared); read, it is the script it leads to.
        fs::remove_file(root.join("usr/local/bin/backup.sh")).unwrap();
        script(root, "usr/local/lib/real.sh", "btrbk run\n");
        link(root, "usr/local/bin/backup.sh", "/usr/local/lib/real.sh");
        assert_eq!(read_refusing(root).at_boot, nothing(), "a link not read");
        assert_eq!(
            at_boot(root).runners[0].script.as_deref(),
            Some("/usr/local/bin/backup.sh")
        );
        // btrbk under another name, a link to it: btrbk, its config the
        // command's -c.
        link(root, "usr/local/bin/snap", "/usr/bin/btrbk");
        write(root, "usr/bin/btrbk", "#!/usr/bin/perl\n# btrbk 0.32\n");
        wrapped(root, "/usr/local/bin/snap -c /opt/s.conf run");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::No, "{:?}", b.reasons);
        assert_eq!(b.runners[0].config.as_deref(), Some("/opt/s.conf"));
        assert_eq!(read_refusing(root).at_boot, nothing(), "a link not read");
        fs::remove_file(root.join("usr/local/bin/backup.sh")).unwrap();
        script(root, "usr/local/bin/backup.sh", "true\n");
        // The same script named directly is.
        wrapped(root, "/usr/local/lib/real.sh");
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
        // A directory, and nothing at all, run nothing.
        wrapped(root, "/usr/local/lib /usr/local/gone.sh");
        assert_eq!(at_boot(root), nothing());
    }

    #[test]
    fn a_path_through_arch_merged_usr_links_is_read_where_arch_points_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        wrapper_root(root, "/bin/backup.sh");
        for (name, target) in [
            ("bin", "usr/bin"),
            ("sbin", "usr/bin"),
            ("lib", "usr/lib"),
            ("lib64", "usr/lib"),
            ("usr/sbin", "bin"),
            ("usr/lib64", "lib"),
        ] {
            link(root, name, target);
        }
        script(root, "usr/bin/backup.sh", "btrbk run\n");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will, "read at /usr/bin/backup.sh");
        assert_eq!(b.runners[0].script.as_deref(), Some("/bin/backup.sh"));
        for (path, rel, link_, target) in [
            ("/sbin/b.sh", "usr/bin/b.sh", "sbin", "usr/bin"),
            ("/lib/x/b.sh", "usr/lib/x/b.sh", "lib", "usr/lib"),
            ("/lib64/b.sh", "usr/lib/b.sh", "lib64", "usr/lib"),
            ("/usr/sbin/b.sh", "usr/bin/b.sh", "usr/sbin", "usr/bin"),
            ("/usr/lib64/b.sh", "usr/lib/b.sh", "usr/lib64", "usr/lib"),
        ] {
            wrapped(root, path);
            script(root, rel, "btrbk run\n");
            assert_eq!(
                at_boot(root).runners[0].script.as_deref(),
                Some(path),
                "{path}"
            );
            // Nothing where Arch's link leads: unread, on another layout it
            // may lead elsewhere, so may, never no; read, nothing is there.
            fs::remove_file(root.join(rel)).unwrap();
            assert_eq!(at_boot(root), nothing(), "{path}");
            assert_eq!(
                read_refusing(root).at_boot.reasons,
                [format!(
                    "backup.service may run btrbk through {path}: {path}: read as /{rel}, \
                     as /{link_} leads to /{target} on Arch, and nothing is there"
                )],
                "{path}"
            );
        }
        // A program link at the end is a program link still: /bin/sh.
        link(root, "usr/bin/sh", "bash");
        wrapped(root, "/bin/sh -c 'echo hello'");
        assert_eq!(at_boot(root), nothing());
        // A link on the way that is not read, and not Arch's: may.
        link(root, "opt", "/srv/opt");
        wrapped(root, "/opt/tools/backup.sh");
        let b = read_refusing(root).at_boot;
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [format!(
                "backup.service may run btrbk through /opt/tools/backup.sh: {}",
                unread("opt")
            )]
        );
        // Read: /srv/opt/tools/backup.sh, here.
        assert_eq!(at_boot(root), nothing());
        script(root, "srv/opt/tools/backup.sh", "btrbk run\n");
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
        // /run is a tmpfs once up, and /var/run and /var/lock lead into it:
        // what the disk holds there is not there to run.
        link(root, "var/run", "../run");
        link(root, "var/lock", "../run/lock");
        script(root, "run/x.sh", "btrbk run\n");
        wrapped(
            root,
            "/usr/bin/true /run/x.sh --pid /var/run/x.pid --lock /var/lock/x",
        );
        assert_eq!(at_boot(root), nothing());
        // ...but a program there may be put there at boot: unknown.
        wrapped(root, "/run/x.sh");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [
                "backup.service may run btrbk through /run/x.sh: /run/x.sh: under /run, which \
              only a running system fills"
            ]
        );
        // A /bin that is a directory, not Arch's link, is read as it is.
        let other = tempfile::tempdir().unwrap();
        wrapper_root(other.path(), "/bin/backup.sh");
        script(other.path(), "bin/backup.sh", "btrbk run\n");
        assert_eq!(at_boot(other.path()).verdict, BootVerdict::Will);
    }

    #[test]
    fn a_command_named_like_btrbk_may_run_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        wrapper_root(root, "/usr/bin/true");
        assert_eq!(at_boot(root), nothing());
        for (command, word) in [
            ("/usr/local/bin/btrbk.sh daily", "/usr/local/bin/btrbk.sh"),
            ("/usr/bin/btrbk-wrapper run", "/usr/bin/btrbk-wrapper"),
            ("/bin/sh -c 'run-btrbk --all'", "run-btrbk"),
        ] {
            wrapped(root, command);
            let b = at_boot(root);
            assert_eq!(b.verdict, BootVerdict::May, "{command}");
            assert_eq!(
                b.reasons,
                [format!("backup.service runs {word}, named for btrbk")],
                "{command}"
            );
        }
        // In a script it runs, too.
        wrapped(root, "/usr/local/bin/backup.sh");
        script(root, "usr/local/bin/backup.sh", "/opt/run-btrbk.sh\n");
        assert_eq!(
            at_boot(root).reasons,
            [
                "backup.service runs /usr/local/bin/backup.sh, which runs /opt/run-btrbk.sh, \
                 named for btrbk"
            ]
        );
        // btrbk's own config argument is btrbk's, not another program.
        wrapped(root, "/usr/bin/btrbk -c /etc/btrbk/btrbk.conf run");
        fs::remove_file(root.join("etc/btrbk/btrbk.conf")).unwrap();
        assert_eq!(at_boot(root).verdict, BootVerdict::No);
    }

    // ---- what a unit pulls in -------------------------------------------------

    #[test]
    fn a_unit_an_enabled_target_pulls_in_runs_when_the_target_starts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        configure(root);
        unit(
            root,
            ETC,
            "backup.target",
            "[Unit]\nDescription=backups\nWants=btrbk.service\n",
        );
        enable(root, ETC, "multi-user.target.wants", "backup.target");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.runners,
            [runner(
                "btrbk.service",
                Some("backup.target"),
                "at every boot",
                None,
                Some(true)
            )]
        );
        assert_eq!(
            b.reasons,
            [format!(
                "backup.target starts btrbk.service, which runs btrbk at every boot, {PRESENT}"
            )]
        );
        // Every setting that pulls a unit in, from a drop-in as from the file.
        unit(root, ETC, "backup.target", "[Unit]\nDescription=backups\n");
        assert_eq!(at_boot(root), nothing());
        let drop_in = "etc/systemd/system/backup.target.d/p.conf";
        for key in ["Requires", "Requisite", "BindsTo", "Upholds"] {
            write(
                root,
                drop_in,
                &format!("[Unit]\n{key}=a.service btrbk.service\n"),
            );
            assert_eq!(at_boot(root).runners.len(), 1, "{key}");
        }
        // An empty assignment does not reset a dependency: systemd.unit(5),
        // "Dependencies (After=, etc.) cannot be reset to an empty list".
        write(
            root,
            "etc/systemd/system/backup.target.d/z.conf",
            "[Unit]\nUpholds=\nWants=\n",
        );
        assert_eq!(at_boot(root).verdict, BootVerdict::Will, "not reset");
        // Only [Unit] pulls a unit in.
        fs::remove_dir_all(root.join("etc/systemd/system/backup.target.d")).unwrap();
        unit(
            root,
            ETC,
            "backup.target",
            "[Install]\nWants=btrbk.service\nAlso=btrbk.service\n",
        );
        assert_eq!(at_boot(root), nothing());
        // A cron daemon pulled in, not enabled itself, runs its tables.
        unit(root, ETC, "backup.target", "[Unit]\nWants=cronie.service\n");
        unit(
            root,
            VENDOR,
            "cronie.service",
            "[Service]\nExecStart=/usr/bin/crond -n\n",
        );
        write(root, "etc/crontab", "0 3 * * * root btrbk run\n");
        assert_eq!(
            at_boot(root).runners,
            [runner(
                "etc/crontab",
                Some("cronie.service"),
                "on its cron schedule",
                None,
                Some(true)
            )]
        );
    }

    #[test]
    fn a_unit_started_when_another_fails_or_succeeds_runs_then() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        configure(root);
        unit(
            root,
            ETC,
            "sync.service",
            "[Unit]\nOnFailure=btrbk.service\n[Service]\nExecStart=/usr/bin/true\n",
        );
        enable(root, ETC, "multi-user.target.wants", "sync.service");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(
            b.runners,
            [runner(
                "btrbk.service",
                Some("sync.service"),
                "if sync.service fails",
                None,
                Some(true)
            )]
        );
        assert_eq!(
            b.reasons,
            [format!(
                "sync.service starts btrbk.service, which runs btrbk if sync.service fails, \
                 {PRESENT}"
            )]
        );
        unit(
            root,
            ETC,
            "sync.service",
            "[Unit]\nOnSuccess=btrbk.service\n[Service]\nExecStart=/usr/bin/true\n",
        );
        assert_eq!(at_boot(root).runners[0].when, "after sync.service succeeds");
        // What a timer's service pulls in runs when that service does.
        fs::remove_dir_all(root.join(ETC)).unwrap();
        unit(root, ETC, "nightly.timer", "[Timer]\nOnBootSec=5min\n");
        unit(
            root,
            ETC,
            "nightly.service",
            "[Unit]\nWants=btrbk.service\n[Service]\nExecStart=/usr/bin/true\n",
        );
        enable(root, ETC, "timers.target.wants", "nightly.timer");
        assert_eq!(
            at_boot(root).runners,
            [runner(
                "btrbk.service",
                Some("nightly.service"),
                "soon after boot (OnBootSec=, OnStartupSec= or OnActiveSec=)",
                None,
                Some(true)
            )]
        );
    }

    #[test]
    fn a_pulled_name_resolves_specifiers_and_templates_as_systemd_does() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configure(root);
        unit(
            root,
            VENDOR,
            "snap@.service",
            "[Service]\nExecStart=/usr/bin/btrbk run\n",
        );
        let job = |root: &Path, pulls: &str| {
            unit(
                root,
                VENDOR,
                "job@.service",
                &format!("[Unit]\n{pulls}\n[Service]\nExecStart=/usr/bin/true\n"),
            );
        };
        job(root, "Requires=snap@%i.service");
        enable(root, ETC, "multi-user.target.wants", "job@daily.service");
        let source = |root: &Path| {
            at_boot(root)
                .runners
                .into_iter()
                .map(|r| r.source)
                .collect::<Vec<_>>()
        };
        assert_eq!(source(root), ["snap@daily.service"]);
        // A template named outright takes the puller's instance.
        job(root, "Requires=snap@.service");
        assert_eq!(source(root), ["snap@daily.service"]);
        for (pulls, want) in [
            ("Wants=snap@%p.service", "snap@job.service"),
            ("Wants=snap@%j.service", "snap@job.service"),
            ("Wants=snap@%N.service", "snap@job@daily.service"),
            ("Wants=snap@%%.service", "snap@%.service"),
        ] {
            job(root, pulls);
            assert_eq!(source(root), [want], "{pulls}");
        }
        // A unit with no instance gives its prefix (systemd 262: Requires=
        // dirmngr@%i.socket in ytest.service is dirmngr@ytest.socket).
        unit(
            root,
            ETC,
            "db-backup.service",
            "[Unit]\nWants=snap@%i.service snap@%j.service\n[Service]\nExecStart=/usr/bin/true\n",
        );
        enable(root, ETC, "multi-user.target.wants", "db-backup.service");
        job(root, "");
        assert_eq!(
            source(root),
            ["snap@db-backup.service", "snap@backup.service"]
        );
        // A specifier that is not the unit's own name's: only that OS knows.
        job(root, "Wants=snap@%H.service");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will, "db-backup's still run");
        assert!(
            b.runners
                .iter()
                .all(|r| r.via.as_deref() == Some("db-backup.service")),
            "{:?}",
            b.runners
        );
        fs::remove_file(root.join("etc/systemd/system/db-backup.service")).unwrap();
        assert_eq!(
            at_boot(root).reasons,
            [
                "job@daily.service pulls in snap@%H.service, which only that OS's systemd can \
              resolve"
            ]
        );
        // What a timer starts is resolved the same way.
        let timers = tempfile::tempdir().unwrap();
        let root = timers.path();
        configure(root);
        unit(
            root,
            VENDOR,
            "snap@.service",
            "[Service]\nExecStart=/usr/bin/btrbk run\n",
        );
        unit(
            root,
            VENDOR,
            "nightly@.timer",
            "[Timer]\nOnCalendar=daily\nUnit=snap@%i.service\n",
        );
        enable(root, ETC, "timers.target.wants", "nightly@pool.timer");
        assert_eq!(source(root), ["snap@pool.service"]);
        unit(
            root,
            VENDOR,
            "nightly@.timer",
            "[Timer]\nOnCalendar=daily\nUnit=snap@%H.service\n",
        );
        assert_eq!(
            at_boot(root).reasons,
            ["nightly@pool.timer starts a unit only that OS's systemd can name"]
        );
    }

    #[test]
    fn the_walk_reads_each_unit_once_and_stops_at_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        configure(root);
        // A cycle ends.
        unit(root, ETC, "a.target", "[Unit]\nWants=b.target\n");
        unit(
            root,
            ETC,
            "b.target",
            "[Unit]\nWants=a.target btrbk.service\nRequires=a.target\n",
        );
        enable(root, ETC, "multi-user.target.wants", "a.target");
        assert_eq!(
            at_boot(root).runners,
            [runner(
                "btrbk.service",
                Some("b.target"),
                "at every boot",
                None,
                Some(true)
            )]
        );
        // The cap: 4096 units are read; one more is unknown.
        fs::remove_dir_all(root.join(ETC)).unwrap();
        let names = |n: usize| {
            (0..n)
                .map(|i| format!("u{i:04}.service"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        enable(root, ETC, "multi-user.target.wants", "big.target");
        // 4096 with default.target, the walk's first.
        unit(
            root,
            ETC,
            "big.target",
            &format!("[Unit]\nWants={}\n", names(4094)),
        );
        assert_eq!(at_boot(root), nothing(), "4096 units");
        unit(
            root,
            ETC,
            "big.target",
            &format!("[Unit]\nWants={}\n", names(4095)),
        );
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            ["more than 4096 units start at boot: the rest were not read"]
        );
    }

    #[test]
    fn the_walk_follows_a_bounded_number_of_starts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Four starts: default.target, a, b, and b's of a, which finds a
        // read before and reached no sooner, so not expanded again. Nothing
        // runs btrbk, so only the bound can make it may.
        unit(root, ETC, "default.target", "[Unit]\nWants=a.service\n");
        unit(root, ETC, "a.service", "[Unit]\nWants=b.service\n");
        unit(root, ETC, "b.service", "[Unit]\nWants=a.service\n");
        let reading = |pulls| read_within(root, FOLLOW, pulls).at_boot;
        let capped = |n: usize| format!("more than {n} starts at boot: the rest were not followed");
        assert_eq!(reading(4), nothing());
        assert_eq!(read_with(root, FOLLOW).at_boot, nothing());
        let short = reading(3);
        assert_eq!(
            (short.verdict, short.reasons),
            (BootVerdict::May, vec![capped(3)])
        );
        // Every reach counts, and the bound is far above many reaches of
        // few units: 50 targets wanting the same 100 units are over 5000.
        let wants: Vec<String> = (0..100).map(|i| format!("u{i}.service")).collect();
        for t in 0..50 {
            let target = format!("t{t}.target");
            unit(
                root,
                ETC,
                &target,
                &format!("[Unit]\nWants={}\n", wants.join(" ")),
            );
            enable(root, ETC, "multi-user.target.wants", &target);
        }
        assert_eq!(read_with(root, FOLLOW).at_boot, nothing());
        assert_eq!(reading(5000).verdict, BootVerdict::May);
    }

    // ---- when a cron job runs -------------------------------------------------

    #[test]
    fn cron_says_when_each_job_runs_after_boot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cron_root(root);
        write(
            root,
            "etc/crontab",
            "@reboot root /usr/bin/btrbk run\n15 1 * * * root btrbk -c /etc/btrbk/btrbk.conf run\n",
        );
        write(
            root,
            "var/spool/cron/root",
            "@reboot btrbk -c /opt/r.conf run\n",
        );
        // anacron runs its own table, and the script directories it names.
        write(
            root,
            "etc/anacrontab",
            "SHELL=/bin/sh\n# 7 25 cron.weekly nice run-parts /etc/cron.weekly\n\
             1\t5\tcron.daily\tnice run-parts /etc/cron.daily\n\
             3 10 snapshots btrbk -c /opt/a.conf run\n",
        );
        script(
            root,
            "etc/cron.daily/btrbk",
            "exec /usr/bin/btrbk -c /opt/d.conf -q run\n",
        );
        script(root, "etc/cron.weekly/btrbk", "btrbk -c /opt/w.conf run\n");
        let catch_up = "soon after boot (anacron catch-up)";
        let got: Vec<(String, String)> = at_boot(root)
            .runners
            .into_iter()
            .map(|r| (r.source, r.when))
            .collect();
        let want: Vec<(String, String)> = [
            ("etc/crontab", "at boot (cron @reboot)"),
            ("etc/crontab", "on its cron schedule"),
            ("etc/anacrontab", catch_up),
            ("etc/cron.daily/btrbk", catch_up),
            ("etc/cron.weekly/btrbk", "on its cron schedule"),
            ("var/spool/cron/root", "at boot (cron @reboot)"),
        ]
        .into_iter()
        .map(|(s, w)| (s.to_string(), w.to_string()))
        .collect();
        assert_eq!(got, want);
    }

    // ---- what that OS mounts from elsewhere ----------------------------------

    /// CachyOS's own layout, the way genfstab writes it, with what fstab(5)
    /// allows besides.
    const CACHYOS_FSTAB: &str = "# <file system> <dir> <type> <options> <dump> <pass>\n\
        UUID=0a /              btrfs subvol=/@,noatime 0 0\n\
        UUID=0a /root          btrfs subvol=/@root 0 0\n\
        UUID=0a /srv/          btrfs subvol=/@srv 0 0\n\
        UUID=0a /my\\040backups btrfs subvol=/@b 0 0\n\
        #UUID=0a /old          btrfs subvol=/@old 0 0\n\
        tmpfs   /tmp           tmpfs defaults,noatime 0 0\n\
        UUID=0b none           swap  defaults 0 0\n\
        broken\n";

    #[test]
    fn a_path_not_here_under_a_mount_point_of_its_fstab_may_be_there() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        wrapper_root(root, "/root/bin/backup.sh");
        // Not here, and nothing mounted over /root: it cannot run.
        assert_eq!(at_boot(root), nothing());
        // CachyOS mounts /root from its own subvolume: the script may be there.
        write(root, "etc/fstab", CACHYOS_FSTAB);
        let r = read(root);
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
        assert_eq!(
            r.at_boot.reasons,
            [
                "backup.service may run btrbk through /root/bin/backup.sh: root/bin/backup.sh: \
                 under /root, which that OS mounts from elsewhere (etc/fstab)"
            ]
        );
        assert_eq!(
            r.problems,
            ["root/bin/backup.sh: under /root, which that OS mounts from elsewhere (etc/fstab)"],
            "not read here"
        );
        // A -c config there; a mount point written with a trailing slash; one
        // with a space, written \040.
        wrapped(root, "/usr/bin/btrbk -c /srv/btrbk.conf run");
        assert_eq!(
            at_boot(root).reasons,
            [
                "backup.service runs btrbk at every boot, its config unknown: srv/btrbk.conf: \
                 under /srv, which that OS mounts from elsewhere (etc/fstab)"
            ]
        );
        wrapped(root, "'/my backups/run.sh'");
        assert_eq!(at_boot(root).verdict, BootVerdict::May);
        // A memory filesystem's mount point hides what is here; a commented
        // line, a swap area and a line of one field mount nothing.
        wrapped(
            root,
            "/tmp/run.sh /old/run.sh /none/run.sh /broken/run.sh /rootx/run.sh",
        );
        assert_eq!(at_boot(root), nothing());
        // What is here under a mount point is not what that OS sees there:
        // the mount shadows it at boot.
        script(root, "root/bin/backup.sh", "btrbk run\n");
        wrapped(root, "/root/bin/backup.sh");
        assert_eq!(at_boot(root).verdict, BootVerdict::May);
        // btrbk's default config under a mount point is unknown, not absent.
        wrapped(root, "/usr/bin/btrbk run");
        fs::remove_file(root.join("etc/btrbk/btrbk.conf")).unwrap();
        assert_eq!(at_boot(root).verdict, BootVerdict::No);
        write(
            root,
            "etc/fstab",
            &format!("{CACHYOS_FSTAB}UUID=0c /etc/btrbk btrfs subvol=/@btrbk 0 0\n"),
        );
        let r = read(root);
        let why = "etc/btrbk/btrbk.conf: under /etc/btrbk, which that OS mounts from elsewhere \
                   (etc/fstab)";
        assert_eq!(r.config, BtrbkConfig::Unreadable { reason: why.into() });
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
        assert_eq!(
            fstab_points(CACHYOS_FSTAB),
            ["root", "srv", "my backups"],
            "/, the commented line, tmpfs, swap and the broken line are not mount points"
        );
    }

    #[test]
    fn an_fstab_that_cannot_be_read_leaves_what_is_not_here_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        wrapper_root(root, "/opt/backup.sh /dev/null /proc/1/fd /sys/x");
        assert_eq!(at_boot(root), nothing());
        link(root, "etc/fstab", "/etc/fstab");
        assert_eq!(
            read(root).problems[0],
            "etc/fstab: more than 8 links",
            "read, it leads round to itself"
        );
        let r = read_refusing(root);
        let why = unread("etc/fstab");
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
        // /dev, /proc and /sys are systemd's, from memory, whatever it says.
        assert_eq!(
            r.at_boot.reasons,
            [format!(
                "backup.service may run btrbk through /opt/backup.sh: opt/backup.sh: not here, \
                 and what that OS mounts over its root is unknown: {why}"
            )]
        );
        assert_eq!(
            r.problems,
            [
                why.clone(),
                format!(
                    "opt/backup.sh: not here, and what that OS mounts over its root is unknown: \
                     {why}"
                )
            ],
            "the fstab, and the script it leaves unknown"
        );
        // btrbk's first config may be there; its second is: present either way.
        assert_eq!(
            r.config,
            BtrbkConfig::Present {
                path: "/etc/btrbk/btrbk.conf".into(),
                size_bytes: CONF.len() as u64
            }
        );
        // Neither here: unknown, never absent.
        fs::remove_file(root.join("etc/btrbk/btrbk.conf")).unwrap();
        assert_eq!(
            read_refusing(root).config,
            BtrbkConfig::Unreadable {
                reason: format!(
                    "etc/btrbk.conf: not here, and what that OS mounts over its root is \
                     unknown: {why}"
                )
            }
        );
        // A FIFO or a device there: unknown the same way.
        fs::remove_file(root.join("etc/fstab")).unwrap();
        let _fifo = Fifo::new(root, "etc/fstab");
        let r = read(root);
        assert_eq!(r.problems[0], "etc/fstab: not a regular file");
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
    }

    // ---- the re-review's fixtures (rr2-fixtures.sh), one test each --------

    /// The vendor targets a boot passes through, as systemd 262 ships them.
    const GRAPHICAL: &str = "[Unit]\nDescription=Graphical Interface\nRequires=multi-user.target\n\
        Wants=display-manager.service\nConflicts=rescue.service rescue.target\n\
        After=multi-user.target rescue.service rescue.target display-manager.service\n\
        AllowIsolate=yes\n";
    const MULTI_USER: &str = "[Unit]\nDescription=Multi-User System\nRequires=basic.target\n\
        Conflicts=rescue.service rescue.target\nAfter=basic.target rescue.service rescue.target\n\
        AllowIsolate=yes\n";
    const BASIC: &str = "[Unit]\nDescription=Basic System\nRequires=sysinit.target\n\
        Wants=sockets.target timers.target paths.target slices.target\n\
        After=sysinit.target sockets.target paths.target slices.target tmp.mount\n\
        RequiresMountsFor=/var /var/tmp\nWants=tmp.mount\n";
    const SYSINIT: &str = "[Unit]\nDescription=System Initialization\n\
        Wants=local-fs.target swap.target\nAfter=local-fs.target swap.target\n\
        Conflicts=emergency.service emergency.target\nBefore=emergency.service emergency.target\n";

    /// rr2-fixtures.sh `base`: a minimal CachyOS root — sshd enabled, the
    /// vendor boot targets with default.target → graphical.target, btrbk
    /// installed and not enabled, Arch's merged-/usr links, /usr/bin/sh → bash.
    fn cachyos(root: &Path) {
        configure(root);
        unit(
            root,
            VENDOR,
            "sshd.service",
            "[Service]\nExecStart=/usr/bin/sshd -D\n",
        );
        enable(root, ETC, "multi-user.target.wants", "sshd.service");
        for (name, text) in [
            ("graphical.target", GRAPHICAL),
            ("multi-user.target", MULTI_USER),
            ("basic.target", BASIC),
            ("sysinit.target", SYSINIT),
        ] {
            unit(root, VENDOR, name, text);
        }
        link(
            root,
            &format!("{VENDOR}/default.target"),
            "graphical.target",
        );
        unit(
            root,
            VENDOR,
            "btrbk.service",
            "[Service]\nType=oneshot\nExecStart=/usr/bin/btrbk run\n",
        );
        link(root, "bin", "usr/bin");
        link(root, "sbin", "usr/bin");
        link(root, "usr/sbin", "bin");
        write(root, "usr/bin/bash", "\x7fELF\x02\x01\x01\0");
        link(root, "usr/bin/sh", "bash");
    }

    /// An enabled `nightly.service` of `text`.
    fn nightly(root: &Path, text: &str) {
        unit(root, VENDOR, "nightly.service", text);
        enable(root, ETC, "multi-user.target.wants", "nightly.service");
    }

    /// The verdict, with its reasons when it is not the one wanted.
    fn verdict_of(root: &Path) -> (BootVerdict, Vec<String>) {
        let b = at_boot(root);
        (b.verdict, b.reasons)
    }

    #[test]
    fn rr2_c0_btrbk_enabled_the_ordinary_way_will_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        enable(root, ETC, "multi-user.target.wants", "btrbk.service");
        assert_eq!(verdict_of(root).0, BootVerdict::Will);
    }

    #[test]
    fn rr2_b1_a_drop_in_on_a_boot_target_that_wants_btrbk_will_run_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        assert_eq!(verdict_of(root), (BootVerdict::No, vec![]));
        write(
            root,
            "etc/systemd/system/multi-user.target.d/backup.conf",
            "[Unit]\nWants=btrbk.service\n",
        );
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will, "{:?}", b.reasons);
        assert_eq!(
            b.runners,
            [runner(
                "btrbk.service",
                Some("multi-user.target"),
                "at every boot",
                None,
                Some(true)
            )]
        );
    }

    #[test]
    fn rr2_b2_a_custom_default_target_that_wants_btrbk_will_run_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        unit(
            root,
            ETC,
            "backup-boot.target",
            "[Unit]\nRequires=multi-user.target\nWants=btrbk.service\n",
        );
        link(
            root,
            "etc/systemd/system/default.target",
            "/etc/systemd/system/backup-boot.target",
        );
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::Will, "{reasons:?}");
    }

    #[test]
    fn rr2_b3_an_etc_copy_of_graphical_target_is_the_one_booted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        unit(
            root,
            ETC,
            "graphical.target",
            &format!("{GRAPHICAL}Wants=btrbk.service\n"),
        );
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::Will, "{reasons:?}");
    }

    #[test]
    fn rr2_b4_a_bare_program_name_is_found_where_systemd_looks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        nightly(root, "[Service]\nType=oneshot\nExecStart=nightly-snap\n");
        script(
            root,
            "usr/local/bin/nightly-snap",
            "exec /usr/bin/btrbk -q run\n",
        );
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::Will, "{reasons:?}");
    }

    #[test]
    fn rr2_b5_a_script_that_a_script_runs_is_read_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        nightly(
            root,
            "[Service]\nType=oneshot\nExecStart=/usr/local/bin/outer.sh\n",
        );
        script(
            root,
            "usr/local/bin/outer.sh",
            "exec /usr/local/bin/inner.sh \"$@\"\n",
        );
        script(root, "usr/local/bin/inner.sh", "exec btrbk run\n");
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::Will, "{reasons:?}");
    }

    #[test]
    fn rr2_b6_an_env_bash_script_reached_through_usr_sbin_will_run() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        nightly(
            root,
            "[Service]\nType=oneshot\nExecStart=/usr/sbin/snap.sh\n",
        );
        write(
            root,
            "usr/bin/snap.sh",
            "#!/usr/bin/env bash\nset -e\nbtrbk -c /etc/btrbk/btrbk.conf run\n",
        );
        assert_eq!(verdict_of(root).0, BootVerdict::Will);
    }

    #[test]
    fn rr2_b7_a_script_a_shell_is_given_is_read_without_a_shebang() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        nightly(
            root,
            "[Service]\nType=oneshot\nExecStart=/bin/sh /usr/local/lib/snap.sh\n",
        );
        write(root, "usr/local/lib/snap.sh", "btrbk run\n");
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::Will, "{reasons:?}");
    }

    #[test]
    fn rr2_b8_an_accepted_connection_s_instance_config_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        unit(
            root,
            VENDOR,
            "snapd.socket",
            "[Socket]\nListenStream=7777\nAccept=yes\n",
        );
        unit(
            root,
            VENDOR,
            "snapd@.service",
            "[Service]\nExecStart=/usr/bin/btrbk -c /etc/btrbk/%i.conf run\n",
        );
        enable(root, ETC, "sockets.target.wants", "snapd.socket");
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::May, "{reasons:?}");
    }

    #[test]
    fn rr2_b9_a_program_under_run_may_be_put_there_at_boot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        nightly(
            root,
            "[Service]\nType=oneshot\n\
             ExecStartPre=/usr/bin/curl -fsSo /run/snap/tool https://example.invalid/tool\n\
             ExecStart=/run/snap/tool\n",
        );
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::May, "{reasons:?}");
    }

    #[test]
    fn rr2_b10_a_script_in_at_under_a_mount_point_is_not_the_one_booted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        write(
            root,
            "etc/fstab",
            "UUID=x / btrfs subvol=/@ 0 0\nUUID=x /root btrfs subvol=/@root 0 0\n",
        );
        nightly(
            root,
            "[Service]\nType=oneshot\nExecStart=/root/bin/snap.sh\n",
        );
        script(root, "root/bin/snap.sh", "true\n");
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::May, "{reasons:?}");
    }

    #[test]
    fn rr2_b11_cron_tables_under_a_mount_point_may_run_btrbk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        write(
            root,
            "etc/fstab",
            "UUID=x / btrfs subvol=/@ 0 0\nUUID=x /var btrfs subvol=/@var 0 0\n",
        );
        unit(
            root,
            VENDOR,
            "cronie.service",
            "[Service]\nExecStart=/usr/bin/crond -n\n",
        );
        enable(root, ETC, "multi-user.target.wants", "cronie.service");
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::May, "{reasons:?}");
    }

    #[test]
    fn rr2_b12_a_mount_unit_s_mount_point_is_one_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        unit(
            root,
            ETC,
            "root.mount",
            "[Mount]\nWhat=/dev/disk/by-label/x\nWhere=/root\nOptions=subvol=/@root\n",
        );
        enable(root, ETC, "local-fs.target.wants", "root.mount");
        nightly(
            root,
            "[Service]\nType=oneshot\nExecStart=/root/bin/snap.sh\n",
        );
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::May, "{reasons:?}");
    }

    fn sddm(root: &Path) {
        unit(
            root,
            VENDOR,
            "sddm.service",
            "[Service]\nExecStart=/usr/bin/sddm\n[Install]\nAlias=display-manager.service\n",
        );
        link(
            root,
            "etc/systemd/system/display-manager.service",
            "/usr/lib/systemd/system/sddm.service",
        );
    }

    #[test]
    fn rr2_c1_an_enable_alias_is_the_unit_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        sddm(root);
        unit(
            root,
            VENDOR,
            "rgb.service",
            "[Unit]\nWants=graphical.target\n[Service]\nExecStart=/usr/bin/true\n",
        );
        enable(root, ETC, "multi-user.target.wants", "rgb.service");
        assert_eq!(verdict_of(root), (BootVerdict::No, vec![]));
    }

    #[test]
    fn rr2_c2_a_display_manager_alias_the_boot_reaches_runs_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        sddm(root);
        assert_eq!(verdict_of(root), (BootVerdict::No, vec![]));
        // The boot reaches it (graphical.target wants it): what sddm runs counts.
        unit(
            root,
            VENDOR,
            "sddm.service",
            "[Service]\nExecStart=/usr/bin/sddm\nExecStartPost=/usr/bin/btrbk run\n",
        );
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will, "{:?}", b.reasons);
    }

    #[test]
    fn rr2_c4_stock_man_db_runs_nothing_whatever_its_arguments_are_on() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        unit(
            root,
            VENDOR,
            "man-db.timer",
            "[Unit]\nDescription=Daily man-db regeneration\n[Timer]\nOnCalendar=daily\n\
             RandomizedDelaySec=12h\nPersistent=true\n[Install]\nWantedBy=timers.target\n",
        );
        unit(
            root,
            VENDOR,
            "man-db.service",
            "[Unit]\nDescription=Daily man-db regeneration\nConditionACPower=true\n\
             [Service]\nType=oneshot\n\
             ExecStart=+/usr/bin/install -d -o root -g root -m 0755 /var/cache/man\n\
             ExecStart=/usr/bin/mandb --quiet\nUser=root\nNice=19\n",
        );
        enable(root, VENDOR, "timers.target.wants", "man-db.timer");
        write(
            root,
            "etc/fstab",
            "UUID=0a / btrfs subvol=/@ 0 0\nUUID=0a /var/cache btrfs subvol=/@cache 0 0\n",
        );
        assert_eq!(verdict_of(root), (BootVerdict::No, vec![]));
    }

    /// rr2-fixtures.sh `svc`: nightly.service of `text`, btrbk's config only
    /// at /etc/btrbk/das.conf, and /usr/local/bin/backup.sh of `script_text`.
    fn das_conf_root(root: &Path, text: &str, script_text: Option<&str>) {
        cachyos(root);
        nightly(root, text);
        fs::remove_file(root.join("etc/btrbk/btrbk.conf")).unwrap();
        write(root, "etc/btrbk/das.conf", CONF);
        if let Some(body) = script_text {
            write(root, "usr/local/bin/backup.sh", body);
        }
    }

    #[test]
    fn rr2_b13_a_continued_script_line_is_one_command() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        das_conf_root(
            root,
            "[Service]\nType=oneshot\nExecStart=/usr/local/bin/backup.sh\n",
            Some("#!/bin/sh\nexec btrbk \\\n    -c /etc/btrbk/das.conf \\\n    run\n"),
        );
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will, "{:?}", b.reasons);
        assert_eq!(b.runners[0].config.as_deref(), Some("/etc/btrbk/das.conf"));
    }

    #[test]
    fn rr2_b14_arguments_forwarded_to_btrbk_leave_its_config_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        das_conf_root(
            root,
            "[Service]\nType=oneshot\nExecStart=/usr/local/bin/backup.sh -c /etc/btrbk/das.conf run\n",
            Some("#!/bin/sh\nexec btrbk \"$@\"\n"),
        );
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::May, "{reasons:?}");
    }

    #[test]
    fn rr2_b15_a_variable_among_btrbk_s_arguments_leaves_its_config_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        das_conf_root(
            root,
            "[Service]\nType=oneshot\nEnvironment=\"OPTS=-c /etc/btrbk/das.conf\"\n\
             ExecStart=/usr/bin/btrbk $OPTS run\n",
            None,
        );
        let (verdict, reasons) = verdict_of(root);
        assert_eq!(verdict, BootVerdict::May, "{reasons:?}");
    }

    // ---- reading a link only where that records nothing --------------------

    /// A fresh directory under /tmp, /dev/shm or the temp dir whose mount's
    /// flags say it records access times (`records`) or not; one that
    /// records must also be seen to move a link's atime on a read, as only
    /// there can a test show that a link was not read.
    fn dir_on(records: bool) -> Option<tempfile::TempDir> {
        let tmp = std::env::temp_dir();
        ["/tmp", "/dev/shm", &tmp.to_string_lossy()]
            .iter()
            .filter_map(|base| tempfile::tempdir_in(base).ok())
            .find(|d| {
                records_access(mount_flags(&fs::File::open(d.path()).unwrap())) == records
                    && (!records || readlink_moves_atime(d.path()))
            })
    }

    /// Whether reading a link in `dir` moves its access time.
    fn readlink_moves_atime(dir: &Path) -> bool {
        let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        std::os::unix::fs::symlink("x", dir.join("probe")).unwrap();
        filetime::set_symlink_file_times(dir.join("probe"), old, old).unwrap();
        fs::read_link(dir.join("probe")).unwrap();
        let moved = link_atime(dir, "probe") != old;
        fs::remove_file(dir.join("probe")).unwrap();
        moved
    }

    /// `rel`'s own access time, never following it.
    fn link_atime(root: &Path, rel: &str) -> filetime::FileTime {
        filetime::FileTime::from_last_access_time(&fs::symlink_metadata(root.join(rel)).unwrap())
    }

    #[test]
    fn a_link_on_a_noatime_mount_is_read_and_its_access_time_stays() {
        // Production's case: every DAS target is mounted noatime.
        let Some(dir) = dir_on(false) else {
            eprintln!("no noatime mount here: the probe's both branches are tested with fakes");
            return;
        };
        let root = dir.path();
        let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        link(root, "l", "/usr/lib/x");
        filetime::set_symlink_file_times(root.join("l"), old, old).unwrap();
        assert_eq!(
            link_target(root, "l", mount_flags).ok().as_deref(),
            Some("/usr/lib/x")
        );
        assert_eq!(link_atime(root, "l"), old, "read, and no access recorded");
    }

    #[test]
    fn a_link_on_a_mount_that_records_access_is_not_read_and_stays_unknown() {
        let Some(dir) = dir_on(true) else {
            eprintln!(
                "no mount here records a link's access: the probe's both branches are tested with fakes"
            );
            return;
        };
        let root = dir.path();
        let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
        link(
            root,
            "etc/systemd/system/default.target",
            "/usr/lib/systemd/system/x.target",
        );
        let rel = "etc/systemd/system/default.target";
        filetime::set_symlink_file_times(root.join(rel), old, old).unwrap();
        assert!(matches!(
            link_target(root, rel, mount_flags),
            Err(LinkErr::Refused(_))
        ));
        assert_eq!(link_atime(root, rel), old, "not read");
        assert_eq!(
            read_with(root, mount_flags).at_boot.reasons,
            [format!("default.target may run btrbk: {}", unread(rel))]
        );
        // The control: reading it here does record the access, so the
        // refusal is what keeps it unwritten.
        fs::read_link(root.join(rel)).unwrap();
        assert_ne!(
            link_atime(root, rel),
            old,
            "readlink writes an access time here"
        );
    }

    #[test]
    fn a_link_resolves_inside_the_root_and_nowhere_else() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "usr/lib/x", "x");
        let at = |rel: &str| locate(root, rel, FOLLOW);
        let file = Located::File("usr/lib/x".into(), 1);
        // Absolute targets are the root's; relative ones the link's dir's.
        link(root, "etc/a", "/usr/lib/x");
        link(root, "etc/b", "../usr/lib/x");
        link(root, "etc/c", "b");
        assert_eq!(
            [at("etc/a"), at("etc/b"), at("etc/c")],
            [file.clone(), file.clone(), file.clone()]
        );
        // `..` stops at the root: one more would leave it.
        link(root, "etc/up", "../../usr/lib/x");
        assert_eq!(
            at("etc/up"),
            Located::Unknown("etc/up: leads out of the root".into())
        );
        // A link on the way, then a file used as a directory.
        link(root, "opt", "usr");
        assert_eq!(at("opt/lib/x"), file);
        assert_eq!(
            at("opt/lib/x/y"),
            Located::Absent {
                rel: "usr/lib/x/y".into(),
                mapped: None
            }
        );
        // /dev/null masks; nothing there is absent, where it would be.
        link(root, "etc/m", "/dev/null");
        assert_eq!(at("etc/m"), Located::Masked);
        link(root, "etc/d", "/nope/x");
        assert_eq!(
            at("etc/d"),
            Located::Absent {
                rel: "nope/x".into(),
                mapped: None
            }
        );
        // Eight links are followed; the ninth is unknown, and so is a loop.
        for n in 1..=8 {
            link(root, &format!("c{n}"), &format!("c{}", n + 1));
        }
        write(root, "c9", "end");
        assert_eq!(at("c1"), Located::File("c9".into(), 3));
        link(root, "c0", "c1");
        assert_eq!(at("c0"), Located::Unknown("c0: more than 8 links".into()));
        link(root, "loop", "loop");
        assert_eq!(
            at("loop"),
            Located::Unknown("loop: more than 8 links".into())
        );
        // A directory, and the root itself.
        assert_eq!(at("usr/lib"), Located::Dir("usr/lib".into()));
        assert_eq!(at(""), Located::Dir(String::new()));
        // A trailing `/` or `.` needs a directory, as path resolution does;
        // `.` and empty components drop out of what is named.
        assert_eq!(at("usr/lib/"), Located::Dir("usr/lib".into()));
        assert_eq!(at("usr/lib/."), Located::Dir("usr/lib".into()));
        for rel in ["usr/lib/x/", "usr/lib/x/.", "etc/a/"] {
            assert_eq!(
                at(rel),
                Located::Absent {
                    rel: "usr/lib/x".into(),
                    mapped: None
                },
                "{rel}"
            );
        }
        for rel in ["nope/./x", "nope//x", "./nope/x/", "nope/x/."] {
            assert_eq!(
                at(rel),
                Located::Absent {
                    rel: "nope/x".into(),
                    mapped: None
                },
                "{rel}"
            );
        }
        // An lstat that fails for another reason than absence is unknown.
        let long = "x".repeat(256);
        assert_eq!(
            at(&format!("usr/{long}")),
            Located::Unknown(format!("usr/{long}: File name too long (os error 36)"))
        );
        // Not read: the path's own last link, or one on the way.
        assert_eq!(
            locate(root, "etc/a", REFUSE),
            Located::Unread {
                why: unread("etc/a"),
                last: true
            }
        );
        assert_eq!(
            locate(root, "opt/lib/x", REFUSE),
            Located::Unread {
                why: unread("opt"),
                last: false
            }
        );
        // A trailing `/` follows it too: not the last, so it may.
        assert_eq!(
            locate(root, "etc/a/", REFUSE),
            Located::Unread {
                why: unread("etc/a"),
                last: false
            }
        );
        // ...but one of Arch's merged-/usr links is taken by name.
        link(root, "lib", "usr/lib");
        assert_eq!(locate(root, "lib/x", REFUSE), file);
        assert_eq!(
            locate(root, "lib/gone", REFUSE),
            Located::Absent {
                rel: "usr/lib/gone".into(),
                mapped: Some("lib")
            }
        );
        assert_eq!(
            locate(root, "lib/gone", FOLLOW),
            Located::Absent {
                rel: "usr/lib/gone".into(),
                mapped: None
            }
        );
    }

    // ---- what the boot starts ------------------------------------------------

    #[test]
    fn the_boot_starts_at_default_target_whatever_enables_what() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cachyos(root);
        // Nothing enabled runs btrbk, but what rescue.target wants does: only
        // when rescue.target starts...
        enable(root, VENDOR, "rescue.target.wants", "btrbk.service");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::Will);
        assert_eq!(b.runners[0].when, "when rescue.target starts");
        // ...which, made the default, is every boot: walked first, from its
        // own dependency directory.
        unit(
            root,
            VENDOR,
            "rescue.target",
            "[Unit]\nDescription=Rescue\n",
        );
        link(
            root,
            "etc/systemd/system/default.target",
            "/usr/lib/systemd/system/rescue.target",
        );
        let b = at_boot(root);
        assert_eq!(
            b.runners,
            [
                runner(
                    "btrbk.service",
                    Some("rescue.target"),
                    "at every boot",
                    None,
                    Some(true)
                ),
                runner(
                    "btrbk.service",
                    None,
                    "when rescue.target starts",
                    None,
                    Some(true)
                ),
            ]
        );
        // Its link not read: what the boot starts is unknown.
        let r = read_refusing(root);
        assert_eq!(r.at_boot.verdict, BootVerdict::Will, "rescue's still");
        assert!(r.at_boot.runners.iter().all(|r| r.when != "at every boot"));
        fs::remove_file(root.join("usr/lib/systemd/system/rescue.target.wants/btrbk.service"))
            .unwrap();
        let r = read_refusing(root);
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
        assert_eq!(
            r.at_boot.reasons,
            [format!(
                "default.target may run btrbk: {}",
                unread("etc/systemd/system/default.target")
            )]
        );
        assert_eq!(r.problems, [unread("etc/systemd/system/default.target")]);
        // The vendor's link not read is as unknown.
        fs::remove_file(root.join("etc/systemd/system/default.target")).unwrap();
        assert_eq!(
            read_refusing(root).at_boot.reasons,
            [format!(
                "default.target may run btrbk: {}",
                unread("usr/lib/systemd/system/default.target")
            )]
        );
        // A default.target that is a file of its own is read as it is.
        fs::remove_file(root.join("usr/lib/systemd/system/default.target")).unwrap();
        unit(root, ETC, "default.target", "[Unit]\nWants=btrbk.service\n");
        assert_eq!(
            read_refusing(root).at_boot.runners,
            [runner(
                "btrbk.service",
                Some("default.target"),
                "at every boot",
                None,
                Some(true)
            )]
        );
    }

    #[test]
    fn a_unit_reached_again_sooner_passes_that_on_to_what_it_starts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        configure(root);
        // a.service is first reached at shutdown, then soon after every boot.
        unit(
            root,
            VENDOR,
            "a.service",
            "[Unit]\nWants=btrbk.service\n[Service]\nExecStart=/usr/bin/true\n",
        );
        enable(root, VENDOR, "shutdown.target.wants", "a.service");
        unit(
            root,
            VENDOR,
            "b.timer",
            "[Timer]\nOnBootSec=1min\nUnit=a.service\n",
        );
        enable(root, VENDOR, "timers.target.wants", "b.timer");
        let whens: Vec<String> = at_boot(root).runners.into_iter().map(|r| r.when).collect();
        assert_eq!(
            whens,
            [
                "when shutdown.target starts",
                "soon after boot (OnBootSec=, OnStartupSec= or OnActiveSec=)"
            ]
        );
        // Reached first at every boot, later by the timer: nothing new.
        enable(root, VENDOR, "multi-user.target.wants", "a.service");
        let whens: Vec<String> = at_boot(root).runners.into_iter().map(|r| r.when).collect();
        assert_eq!(
            whens,
            ["at every boot"],
            "the timer's is less urgent than ever"
        );
    }

    // ---- a script's lines as sh reads them -----------------------------------

    #[test]
    fn a_script_s_lines_are_its_commands_as_sh_reads_them() {
        let lines = |text: &str| shell_lines(text);
        assert_eq!(
            lines("a \\\n  b \\\n  c\nd\n"),
            Ok(vec!["a   b   c".to_string(), "d".to_string()])
        );
        // A quote runs across lines; a comment starts a word.
        assert_eq!(
            lines("echo 'x\ny' # btrbk run\nz#not\n"),
            Ok(vec!["echo 'x y' ".to_string(), "z#not".to_string()])
        );
        assert_eq!(
            lines("# btrbk run\n  # also\n"),
            Ok(vec![String::new(), "  ".to_string()])
        );
        assert_eq!(
            lines("a \"b \\\" c\"\n"),
            Ok(vec!["a \"b \\\" c\"".to_string()])
        );
        assert_eq!(lines("echo \\# x\n"), Ok(vec!["echo \\# x".to_string()]));
        assert_eq!(lines("echo \\' x\n"), Ok(vec!["echo \\' x".to_string()]));
        // In double quotes only a backslash escapes: a quote after any
        // other character closes them.
        assert_eq!(
            lines("echo \"x\" ; btrbk run\n"),
            Ok(vec!["echo \"x\" ; btrbk run".to_string()])
        );
        // A here-document's lines are data.
        assert_eq!(
            lines("cat <<EOF >f\nbtrbk run\ndon't\nEOF\nnext\n"),
            Ok(vec!["cat <<EOF >f".to_string(), "next".to_string()])
        );
        assert_eq!(
            lines("cat <<- 'END'\n\tbtrbk run\n\tEND\nafter\n"),
            Ok(vec!["cat <<- 'END'".to_string(), "after".to_string()])
        );
        assert_eq!(
            lines("cat <<<x\nbtrbk run\n"),
            Ok(vec!["cat <<<x".into(), "btrbk run".into()])
        );
        // What cannot end is unknown.
        assert_eq!(lines("echo 'x\n"), Err("it ends inside a quote".into()));
        assert_eq!(
            lines("btrbk \\\n"),
            Err("its last line ends in a backslash".into())
        );
        // In a unit: unknown, so may.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        wrapper_root(root, "/usr/local/bin/backup.sh");
        script(
            root,
            "usr/local/bin/backup.sh",
            "echo \"unfinished\nbtrbk -c /opt/x.conf run\n",
        );
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [
                "backup.service may run btrbk through /usr/local/bin/backup.sh: \
              /usr/local/bin/backup.sh: it ends inside a quote"
            ]
        );
        // A comment line naming btrbk runs nothing.
        script(
            root,
            "usr/local/bin/backup.sh",
            "# btrbk run\ntrue # btrbk run\n",
        );
        assert_eq!(at_boot(root), nothing());
    }

    // ---- a program named without its path -------------------------------------

    #[test]
    fn a_bare_program_is_found_where_systemd_looks_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        wrapper_root(root, "snap --daily");
        assert_eq!(at_boot(root), nothing(), "nowhere: the command fails");
        script(root, "usr/bin/snap", "btrbk -c /opt/b.conf run\n");
        script(root, "usr/local/bin/snap", "btrbk run\n");
        let b = at_boot(root);
        assert_eq!(b.runners.len(), 1, "the first found");
        assert_eq!(b.runners[0].script.as_deref(), Some("/usr/local/bin/snap"));
        // Merged sbin (Arch): no sbin directory is searched.
        script(root, "usr/local/sbin/snap", "btrbk -c /opt/s.conf run\n");
        assert_eq!(at_boot(root).runners[0].config, None);
        fs::remove_file(root.join("usr/local/bin/snap")).unwrap();
        assert_eq!(
            at_boot(root).runners[0].config.as_deref(),
            Some("/opt/b.conf")
        );
        // Split sbin: usr/local/sbin first, and usr/sbin before usr/bin.
        fs::create_dir_all(root.join("usr/sbin")).unwrap();
        assert_eq!(
            at_boot(root).runners[0].config.as_deref(),
            Some("/opt/s.conf")
        );
        fs::remove_file(root.join("usr/local/sbin/snap")).unwrap();
        script(root, "usr/sbin/snap", "btrbk -c /opt/u.conf run\n");
        assert_eq!(
            at_boot(root).runners[0].config.as_deref(),
            Some("/opt/u.conf")
        );
        fs::remove_file(root.join("usr/sbin/snap")).unwrap();
        // ...and usr/local/bin before usr/bin, there too.
        assert_eq!(
            at_boot(root).runners[0].config.as_deref(),
            Some("/opt/b.conf")
        );
        script(root, "usr/local/bin/snap", "btrbk -c /opt/l.conf run\n");
        assert_eq!(
            at_boot(root).runners[0].config.as_deref(),
            Some("/opt/l.conf")
        );
        fs::remove_file(root.join("usr/local/bin/snap")).unwrap();
        script(root, "usr/local/sbin/snap", "btrbk -c /opt/s.conf run\n");
        // A directory searched that that OS mounts: it may be there.
        fs::remove_dir_all(root.join("usr/sbin")).unwrap();
        fs::remove_dir_all(root.join("usr/local")).unwrap();
        fs::remove_file(root.join("usr/bin/snap")).unwrap();
        write(root, "etc/fstab", "x /usr/local btrfs subvol=@local 0 0\n");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [
                "backup.service may run btrbk through /usr/local/bin/snap: usr/local/bin/snap: \
              under /usr/local, which that OS mounts from elsewhere (etc/fstab)"
            ]
        );
        // In a script, and in a cron line, the same.
        fs::remove_file(root.join("etc/fstab")).unwrap();
        script(root, "usr/bin/inner", "btrbk run\n");
        wrapped(root, "/usr/local/bin/outer.sh");
        script(root, "usr/local/bin/outer.sh", "FOO=1 inner\n");
        assert_eq!(
            at_boot(root).runners[0].script.as_deref(),
            Some("/usr/bin/inner")
        );
    }

    // ---- what that OS mounts -------------------------------------------------

    #[test]
    fn a_mount_unit_mounts_its_name_s_path_unless_it_lives_in_memory() {
        for (name, point) in [
            ("root.mount", Some("root")),
            ("var-lib-x.mount", Some("var/lib/x")),
            ("srv-my\\x2dsite.automount", Some("srv/my-site")),
            ("-.mount", None),
        ] {
            assert_eq!(unit_mount_point(name).as_deref(), point, "{name}");
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        wrapper_root(root, "/opt/x/backup.sh");
        assert_eq!(at_boot(root), nothing());
        unit(
            root,
            VENDOR,
            "opt-x.mount",
            "[Mount]\nWhat=tmpfs\nWhere=/opt/x\nType=tmpfs\n",
        );
        assert_eq!(at_boot(root), nothing(), "a tmpfs holds nothing at boot");
        unit(
            root,
            VENDOR,
            "opt-x.mount",
            "[Mount]\nWhat=/dev/sdz1\nWhere=/opt/x\nType=btrfs\n",
        );
        assert_eq!(
            at_boot(root).verdict,
            BootVerdict::May,
            "a disk may hold it"
        );
        fs::remove_file(root.join("usr/lib/systemd/system/opt-x.mount")).unwrap();
        // An automount, or a mount only enabled by name, counts too.
        enable(root, ETC, "local-fs.target.wants", "opt.automount");
        assert_eq!(at_boot(root).verdict, BootVerdict::May);
        // What systemd mounts from memory is no mount point here.
        fs::remove_dir_all(root.join("etc/systemd/system/local-fs.target.wants")).unwrap();
        unit(root, VENDOR, "run-x.mount", "[Mount]\nWhere=/run/x\n");
        wrapped(root, "/usr/bin/true /run/x/y");
        assert_eq!(at_boot(root), nothing());
    }

    // ---- the trees systemd 262 reads -------------------------------------------

    #[test]
    fn system_control_and_system_attached_are_unit_trees_in_their_places() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package_btrbk(root);
        configure(root);
        // A portable service's unit, attached.
        unit(
            root,
            "etc/systemd/system.attached",
            "snap.service",
            "[Service]\nExecStart=/usr/bin/btrbk run\n",
        );
        enable(root, ETC, "multi-user.target.wants", "snap.service");
        assert_eq!(at_boot(root).runners[0].source, "snap.service");
        // `systemctl set-property`'s drop-ins come first of all.
        write(
            root,
            "etc/systemd/system.control/snap.service.d/50-x.conf",
            "[Service]\nExecStart=\n",
        );
        assert_eq!(at_boot(root), nothing());
        // ...and the unit in /etc outranks the attached one.
        fs::remove_dir_all(root.join("etc/systemd/system.control")).unwrap();
        unit(
            root,
            ETC,
            "snap.service",
            "[Service]\nExecStart=/usr/bin/true\n",
        );
        assert_eq!(at_boot(root), nothing());
        assert_eq!(
            UNIT_TREES,
            [
                "etc/systemd/system.control",
                "etc/systemd/system",
                "etc/systemd/system.attached",
                "usr/local/lib/systemd/system",
                "usr/lib/systemd/system",
            ]
        );
    }

    // ---- each table entry that decides -----------------------------------------

    #[test]
    fn every_cron_daemon_listed_reads_cron() {
        for daemon in [
            "cronie.service",
            "crond.service",
            "cron.service",
            "dcron.service",
            "fcron.service",
            "anacron.service",
            "anacron.timer",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            configure(root);
            write(root, "etc/crontab", "0 3 * * * root btrbk run\n");
            enable(root, ETC, "multi-user.target.wants", daemon);
            assert_eq!(at_boot(root).verdict, BootVerdict::Will, "{daemon}");
        }
    }

    #[test]
    fn every_memory_filesystem_listed_holds_nothing_at_boot() {
        for kind in ["tmpfs", "ramfs", "proc", "sysfs", "devtmpfs", "devpts"] {
            assert_eq!(
                fstab_points(&format!("x /opt {kind} defaults 0 0\n")),
                Vec::<String>::new(),
                "{kind}"
            );
        }
        assert_eq!(fstab_points("x /opt btrfs defaults 0 0\n"), ["opt"]);
    }

    #[test]
    fn every_volatile_tree_ignores_an_argument_and_doubts_a_program() {
        for dir in ["run", "var/run", "var/lock", "dev", "proc", "sys"] {
            let d = tempfile::tempdir().unwrap();
            let root = d.path();
            wrapper_root(root, &format!("/usr/bin/true /{dir}/x"));
            script(root, &format!("{dir}/x"), "btrbk run\n");
            assert_eq!(at_boot(root), nothing(), "{dir}: an argument");
            wrapped(root, &format!("/{dir}/x"));
            assert_eq!(at_boot(root).verdict, BootVerdict::May, "{dir}: a program");
        }
    }

    #[test]
    fn every_dependency_directory_and_cron_place_listed_is_read() {
        for suffix in ["wants", "requires", "upholds"] {
            let d = tempfile::tempdir().unwrap();
            let root = d.path();
            package_btrbk(root);
            configure(root);
            enable(
                root,
                ETC,
                &format!("multi-user.target.{suffix}"),
                "btrbk.service",
            );
            assert_eq!(at_boot(root).verdict, BootVerdict::Will, "{suffix}");
        }
        for (rel, text) in [
            ("etc/cron.d/x", "0 3 * * * root btrbk run\n"),
            ("etc/cron.hourly/x", "btrbk run\n"),
            ("etc/cron.daily/x", "btrbk run\n"),
            ("etc/cron.weekly/x", "btrbk run\n"),
            ("etc/cron.monthly/x", "btrbk run\n"),
            ("var/spool/cron/root", "0 3 * * * btrbk run\n"),
            ("var/spool/cron/crontabs/root", "0 3 * * * btrbk run\n"),
            ("etc/crontab", "0 3 * * * root btrbk run\n"),
            ("etc/anacrontab", "1 5 job btrbk run\n"),
        ] {
            let d = tempfile::tempdir().unwrap();
            let root = d.path();
            cron_root(root);
            write(root, rel, text);
            assert_eq!(at_boot(root).verdict, BootVerdict::Will, "{rel}");
        }
        // A commented line runs nothing, however many fields it has.
        let d = tempfile::tempdir().unwrap();
        cron_root(d.path());
        write(d.path(), "etc/crontab", "# 0 3 * * * root btrbk run\n");
        assert_eq!(at_boot(d.path()), nothing());
        // anacron catches up each script directory its table names.
        for dir in ["cron.daily", "cron.weekly", "cron.monthly"] {
            let d = tempfile::tempdir().unwrap();
            let root = d.path();
            cron_root(root);
            write(
                root,
                "etc/anacrontab",
                &format!("1 5 j run-parts /etc/{dir}\n"),
            );
            script(root, &format!("etc/{dir}/x"), "btrbk run\n");
            assert_eq!(
                at_boot(root).runners[0].when,
                "soon after boot (anacron catch-up)",
                "{dir}"
            );
        }
    }

    #[test]
    fn every_shell_interpreter_and_wrapper_listed_runs_what_it_is_given() {
        let programs = |line: &str| -> Vec<String> {
            launches(&words(line), Grammar::Systemd)
                .into_iter()
                .filter_map(|l| match l {
                    Launch::Program { word, .. } => Some(word),
                    Launch::Inline(_) => None,
                })
                .collect()
        };
        // systemd's grammar: sh's keywords (`time`, `exec`'s neighbours) are
        // programs there, as /usr/bin/time is.
        for shell in [
            "sh", "bash", "dash", "zsh", "ksh", "mksh", "ash", "yash", ".", "source",
        ] {
            assert_eq!(programs(&format!("{shell} /x")), [shell, "/x"], "{shell}");
        }
        for interpreter in [
            "python", "python2", "python3", "perl", "ruby", "node", "php", "lua",
        ] {
            assert_eq!(
                programs(&format!("{interpreter} /x")),
                [interpreter, "/x"],
                "{interpreter}"
            );
        }
        let inline = |line: &str| -> Vec<String> {
            launches(&words(line), Grammar::Systemd)
                .into_iter()
                .filter_map(|l| match l {
                    Launch::Inline(line) => Some(line),
                    Launch::Program { .. } => None,
                })
                .collect()
        };
        // Past a shell's options, and the values of those that take one, to
        // its script; a cluster with `c` runs the next word as a line.
        for line in [
            "sh -e /x",
            "sh +x /x",
            "sh - /x",
            "sh -o pipefail /x",
            "sh +o posix /x",
            "bash -O extglob /x",
            "bash +O extglob /x",
            "bash --rcfile /r /x",
            "bash --init-file /r /x",
            "sh -eu -o pipefail /x",
        ] {
            assert_eq!(programs(line)[1..], ["/x"], "{line}");
        }
        for line in ["sh -c x", "sh -ec x", "bash -xc x", "sh -e -c x"] {
            assert_eq!(
                (programs(line).len(), inline(line)),
                (1, vec!["x".into()]),
                "{line}"
            );
        }
        // Past an interpreter's options and their values; code or a module
        // given on the line runs no file.
        for line in [
            "python3 -u /x",
            "python3 -W ignore /x",
            "python3 -X dev /x",
            "perl -I /lib /x",
            "perl -M strict /x",
            "ruby -r json /x",
            "node --require m /x",
        ] {
            assert_eq!(programs(line)[1..], ["/x"], "{line}");
        }
        // Past a wrapper's options, the values of those that take one, its
        // own operands and env's assignments; `--` ends its options; an
        // option whose value is a command line runs that line.
        for line in [
            "env -i A=1 B=2 /x",
            "env - /x",
            "env -u NAME /x",
            "env --chdir /d /x",
            "env -- /x",
            "nice -n 5 /x",
            "ionice -c 2 -n 7 /x",
            "chrt -T 1 5 /x",
            "flock -w 3 /lock /x",
            "timeout -s KILL -k 5 10 /x",
            "timeout -- 10 /x",
            "stdbuf -o L /x",
            "time -f fmt /x",
            "exec -a name /x",
            "sudo -u root --group wheel /x",
            "doas -u root /x",
            "runuser -u nobody /x",
            "su - -s /bin/sh root /x",
            "xargs -n 1 -P 4 /x",
            "setsid -f /x",
        ] {
            assert_eq!(programs(line)[1..], ["/x"], "{line}");
        }
        for line in [
            "flock /lock -c x",
            "flock --command x",
            "env -S x",
            "env --split-string=x",
            "su - root -c x",
            "runuser -u nobody --command x",
        ] {
            assert_eq!(
                (programs(line).len(), inline(line)),
                (1, vec!["x".into()]),
                "{line}"
            );
        }
        for line in [
            "python3 -c x",
            "perl -e x",
            "perl -E x",
            "node --eval x",
            "python3 -m x",
            "python3 -u -m x",
        ] {
            assert_eq!(programs(line).len(), 1, "{line}");
        }
        for (wrapper, operands) in [
            ("env", 0),
            ("nice", 0),
            ("ionice", 0),
            ("chrt", 1),
            ("taskset", 1),
            ("flock", 1),
            ("timeout", 1),
            ("nohup", 0),
            ("setsid", 0),
            ("stdbuf", 0),
            ("time", 0),
            ("exec", 0),
            ("sudo", 0),
            ("doas", 0),
            ("runuser", 0),
            ("su", 1),
            ("xargs", 0),
            ("systemd-run", 0),
            ("systemd-inhibit", 0),
            ("systemd-cat", 0),
            ("setpriv", 0),
            ("unshare", 0),
            ("nsenter", 0),
            ("cgexec", 0),
            ("dbus-run-session", 0),
        ] {
            let line = format!("{wrapper} {}/x", "o ".repeat(operands));
            assert_eq!(programs(&line), [wrapper, "/x"], "{wrapper}");
        }
        // sh's keywords are passed over; a loop's or case's header runs nothing.
        let shell = |line: &str| -> Vec<String> {
            launches(&words(line), Grammar::Shell)
                .into_iter()
                .filter_map(|l| match l {
                    Launch::Program { word, .. } => Some(word),
                    Launch::Inline(_) => None,
                })
                .collect()
        };
        for keyword in [
            "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "esac", "!", "{",
            "}", "time", "coproc",
        ] {
            assert_eq!(shell(&format!("{keyword} /x")), ["/x"], "{keyword}");
        }
        for header in ["for", "select", "case", "function"] {
            assert_eq!(
                shell(&format!("{header} /x")),
                Vec::<String>::new(),
                "{header}"
            );
        }
    }

    #[test]
    fn a_systemd_command_s_prefixes_and_argv0_are_not_its_arguments() {
        let launch = |line: &str| launches(&words(line), Grammar::Systemd);
        let program = |word: &str, role: Role, literal: bool, args: &[&str]| Launch::Program {
            word: word.into(),
            role,
            literal,
            args: args.iter().map(|a| a.to_string()).collect(),
        };
        assert_eq!(
            launch("/x a b"),
            [program("/x", Role::Program, true, &["a", "b"])]
        );
        // `@` makes the next word argv[0]: not an argument, nor the script.
        for prefixed in ["@/x", "-@/x", "@-/x", "+@:/x"] {
            assert_eq!(
                launch(&format!("{prefixed} name a")),
                [program("/x", Role::Program, true, &["a"])],
                "{prefixed}"
            );
        }
        assert_eq!(launch("@/x"), [program("/x", Role::Program, true, &[])]);
        let shell = [
            program("/bin/sh", Role::Program, true, &["-c", "btrbk run"]),
            Launch::Inline("btrbk run".into()),
        ];
        assert_eq!(launch("-@/bin/sh mysh -c 'btrbk run'"), shell);
        assert_eq!(launch("/bin/sh -c 'btrbk run'"), shell);
        // What a shell or a wrapper runs is read with its own arguments.
        assert_eq!(
            launch("/bin/sh /s.sh a ; -/usr/bin/env A=1 /y b"),
            [
                program("/bin/sh", Role::Program, true, &["/s.sh", "a"]),
                program("/s.sh", Role::Script, false, &["a"]),
                program("/usr/bin/env", Role::Program, true, &["A=1", "/y", "b"]),
                program("/y", Role::Program, false, &["b"]),
            ]
        );
    }

    #[test]
    fn every_target_reached_only_to_stop_sleep_or_start_from_initrd_says_so() {
        for prefix in [
            "shutdown.",
            "reboot.",
            "halt.",
            "poweroff.",
            "kexec.",
            "final.",
            "sleep.",
            "suspend",
            "hibernate.",
            "hybrid-sleep.",
            "initrd",
            "emergency.",
            "rescue.",
            "factory-reset.",
        ] {
            let target = format!("{prefix}x.target");
            let dirs = [format!("etc/systemd/system/{target}.wants")];
            assert_eq!(boot_when(&dirs), When::Starts(target.clone()), "{prefix}");
        }
        for kind in [
            "service",
            "socket",
            "target",
            "timer",
            "path",
            "mount",
            "automount",
            "swap",
            "slice",
            "scope",
            "device",
        ] {
            assert!(is_unit_name(&format!("x.{kind}")), "{kind}");
        }
        assert!(!is_unit_name("x.conf") && !is_unit_name(".service") && !is_unit_name("x"));
        // A unit of a type that runs nothing, named for btrbk, may; one of a
        // type whose file says what it runs is read instead.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        configure(root);
        for kind in ["service", "socket", "timer", "path"] {
            unit(
                root,
                ETC,
                &format!("btrbk-x.{kind}"),
                "[Unit]\nDescription=x\n",
            );
            enable(
                root,
                ETC,
                "multi-user.target.wants",
                &format!("btrbk-x.{kind}"),
            );
        }
        let reasons = at_boot(root).reasons;
        assert!(
            reasons.iter().all(|r| !r.ends_with(" is named for btrbk")),
            "{reasons:?}"
        );
        unit(root, ETC, "btrbk-x.target", "[Unit]\nDescription=x\n");
        enable(root, ETC, "multi-user.target.wants", "btrbk-x.target");
        assert!(
            at_boot(root)
                .reasons
                .contains(&"btrbk-x.target is named for btrbk".to_string())
        );
    }
}
