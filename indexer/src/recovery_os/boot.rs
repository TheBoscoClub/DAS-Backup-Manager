//! Whether booting a recovery OS would run btrbk (bd DAS-Backup-Manager-1yg)
//! — on bare metal or in the update VM — and when.
//!
//! systemd starts at boot what the dependency directories of its unit trees
//! name: `<unit>.wants/`, `.requires/` and `.upholds/` under
//! `etc/systemd/system`, where `systemctl enable` puts its links, and under
//! `usr/local/lib/systemd/system` and `usr/lib/systemd/system`, where
//! packages enable their own. Each entry is a symlink to a unit file; only
//! the names are read. From those names a walk reads each unit by name, in
//! systemd's precedence order — an instance falls back to its template —
//! with its drop-ins, and goes on to what it starts: the unit a timer or a
//! path unit names (`Unit=`, else its own name), the service a socket starts
//! when something connects to it (`Service=`, else its own name; the
//! template, for `Accept=yes`), and every unit its `[Unit]` section pulls in
//! (`Wants=`, `Requires=`, `Requisite=`, `BindsTo=`, `Upholds=`,
//! `OnSuccess=`, `OnFailure=`), with the specifiers that come from a unit's
//! own name resolved and a template instantiated as systemd does. Each unit
//! is read once, at most [`MAX_UNITS`] in all, and each way it is started is
//! reported.
//!
//! A unit runs btrbk if an `Exec…=` command left after its drop-ins names it
//! (`btrbk`, a path ending in it, or a `sh -c` script that does), or names
//! by an absolute path a `#!` script with a line that does — read one level
//! deep: what that script runs is not read. A word that only contains btrbk
//! (`btrbk.sh`, `run-btrbk`) means it may. When a cron daemon is among the
//! units, its tables and scripts are read the same way. Each runner's config
//! is the `-c` it passes, else btrbk's default; it is only `lstat`ed.
//!
//! Everything is read with the parent module's rules: `O_NOATIME`, no
//! symlink followed, absent told from unreadable, and nothing written.
//! Reading a link would update its access time, so a link is never read,
//! which means:
//!
//! - a unit file linked into `/etc` by `systemctl link`, one masked there by
//!   `systemctl mask` (a link to `/dev/null`), and an alias `systemctl
//!   enable` made there (`display-manager.service`) cannot be told apart:
//!   each counts as "may run btrbk";
//! - a symlink among the vendor units is a package's alias (Arch ships
//!   `dbus.service` → `dbus-broker.service`) and is skipped, unless its name
//!   is btrbk's;
//! - a program named through a link (`/usr/bin/sh` → `bash`) is not looked
//!   into, and a path through one of Arch's merged-`/usr` links is read
//!   where Arch's `filesystem` package points that link ([`MERGED_USR`]).
//!
//! The root read is that OS's `@` subvolume alone. A path not here that its
//! `etc/fstab` mounts from elsewhere (`/root`, `/home`, `/srv` on CachyOS),
//! or anything not here when that fstab cannot be read, may be there once it
//! is up, so it is unknown; what systemd mounts from memory ([`VOLATILE`])
//! holds nothing to run.
//!
//! Not read: scripts a script runs, programs that are not `#!` scripts, a
//! script named through a variable or without its path, units generators
//! create at boot, user units and user managers, and `/etc/rc.local`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::{
    BTRBK_CONFIGS, BootVerdict, BtrbkAtBoot, BtrbkConfig, BtrbkRunner, EnabledUnit, EnabledUnits,
    ReadErr, dir_names_noatime, io_error, open_in_root, read_in_root, resolve_in_root,
};

/// The persistent unit trees, highest precedence first (systemd.unit(5),
/// "System Unit Search Path"; the ones under /run are empty at rest).
pub(super) const UNIT_TREES: [&str; 3] = [
    "etc/systemd/system",
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
/// The most units the walk reads. The host this was written on starts 413
/// from 272 enabled names; past the cap the rest are unknown.
const MAX_UNITS: usize = 4096;
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
/// The script directories anacron runs, when its table names them.
const ANACRON_DIRS: [&str; 3] = ["cron.daily", "cron.weekly", "cron.monthly"];
/// Arch's merged-`/usr` links, as its `filesystem` package makes them: a
/// path a command names through one is read where the link leads on Arch,
/// taken by name — a link is never read. Nothing there is unknown, not
/// absent: on another layout the link may lead elsewhere.
const MERGED_USR: [(&str, &str); 6] = [
    ("bin", "usr/bin"),
    ("sbin", "usr/bin"),
    ("lib", "usr/lib"),
    ("lib64", "usr/lib"),
    ("usr/sbin", "usr/bin"),
    ("usr/lib64", "usr/lib"),
];
/// What systemd mounts from memory before anything runs — `/run` (a tmpfs
/// `/var/run` and `/var/lock` lead into), `/dev`, `/proc` and `/sys` — over
/// whatever the disk holds there: nothing under them is there to run.
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
    File(u64),
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
        Ok(m) if m.is_file() => Entry::File(m.len()),
        Ok(m) if m.is_dir() => Entry::Dir,
        Ok(_) => Entry::Other,
        Err(e) => match io_error(rel, &e) {
            ReadErr::Absent => Entry::Absent,
            ReadErr::Unreadable(why) => Entry::Unreadable(why),
        },
    }
}

/// A file read for this analysis.
enum FileRead {
    Text(String),
    Absent,
    /// A directory, a FIFO, a device: not a file anything here reads.
    NotFile,
    Symlink(String),
    Unknown(String),
}

/// Read `rel` with [`read_in_root`] (`O_NOATIME`, no link followed), unless
/// it is larger than [`MAX_READ_BYTES`]: that is unknown, not read in part.
fn read_file(root: &Path, rel: &str) -> FileRead {
    match entry_at(root, rel) {
        Entry::File(len) if len > MAX_READ_BYTES => {
            FileRead::Unknown(format!("{rel}: larger than 64 KiB, not read"))
        }
        Entry::File(_) => match read_in_root(root, rel) {
            Ok(text) => FileRead::Text(text),
            Err(ReadErr::Absent) => FileRead::Absent,
            Err(ReadErr::Unreadable(why)) => FileRead::Unknown(why),
        },
        Entry::Dir | Entry::Other => FileRead::NotFile,
        Entry::Symlink(why) => FileRead::Symlink(why),
        Entry::Absent => FileRead::Absent,
        Entry::Unreadable(why) => FileRead::Unknown(why),
    }
}

/// Where that OS mounts other filesystems over its root at boot. The root
/// read here is its `@` subvolume alone, so a path under such a mount point
/// is not here even when that OS has it — CachyOS mounts `/root`, `/home`,
/// `/srv`, `/var/log` and `/var/cache` from subvolumes of their own.
enum Mounts {
    /// The mount points of its fstab, relative to the root: all but `/` and
    /// the [`MEMORY_FS`] ones.
    Points(Vec<String>),
    /// Its fstab is there and could not be read.
    Unknown(String),
}

impl Mounts {
    /// Why `rel`, absent from this root, may be there once that OS is up:
    /// it is under one of the mount points, or they are unknown.
    fn hides(&self, rel: &str) -> Option<String> {
        match self {
            Mounts::Points(points) => points
                .iter()
                .find(|p| {
                    rel.strip_prefix(p.as_str())
                        .is_some_and(|r| r.starts_with('/'))
                })
                .map(|p| {
                    format!("{rel}: under /{p}, which that OS mounts from elsewhere (etc/fstab)")
                }),
            Mounts::Unknown(why) => Some(format!(
                "{rel}: not here, and what that OS mounts over its root is unknown: {why}"
            )),
        }
    }
}

/// That OS's [`Mounts`], from its `etc/fstab`: none when it has none.
fn read_mounts(root: &Path) -> Mounts {
    match read_file(root, FSTAB) {
        FileRead::Text(text) => Mounts::Points(fstab_points(&text)),
        FileRead::Absent => Mounts::Points(Vec::new()),
        FileRead::NotFile => Mounts::Unknown(format!("{FSTAB}: not a regular file")),
        FileRead::Symlink(why) | FileRead::Unknown(why) => Mounts::Unknown(why),
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

/// What an absolute path a command names is, for whether it runs btrbk.
enum Program {
    /// A `#!` script, read whole.
    Script(String),
    /// Nothing this reads: a binary or another file with no `#!` line, a
    /// program link, a directory, a node, or nothing at all.
    Other,
    /// It could not be read here: a script over the cap or unreadable, a link
    /// on the way that is not one of [`MERGED_USR`], or nothing here where it
    /// may be once that OS is up — under a mount point of its fstab, or where
    /// Arch's link leads on another layout.
    Unknown(String),
}

/// `path`, an absolute path a command names, under the root. A component on
/// the way that is one of Arch's [`MERGED_USR`] links is read where Arch
/// points it; one under [`VOLATILE`] is not there to run; one that is not
/// here, under a mount point of `mounts`, is unknown.
fn program_at(root: &Path, mounts: &Mounts, path: &str) -> Program {
    // `/`, and `/etc/` written as a directory: directories, whatever is there.
    let rel = path.trim_matches('/');
    let under = |dir: &str| rel.strip_prefix(dir).is_some_and(|r| r.starts_with('/'));
    if rel.is_empty() || VOLATILE.iter().any(|dir| under(dir)) {
        return Program::Other;
    }
    let link = MERGED_USR
        .iter()
        .find(|(link, _)| under(link) && matches!(entry_at(root, link), Entry::Symlink(_)));
    let rel = match link {
        Some((link, target)) => format!("{target}{}", &rel[link.len()..]),
        None => rel.to_string(),
    };
    match (entry_at(root, &rel), link) {
        (Entry::File(_), _) => read_script(root, &rel),
        (Entry::Absent, Some((link, target))) => Program::Unknown(format!(
            "{path}: read as /{rel}, as /{link} leads to /{target} on Arch, and nothing is there"
        )),
        (Entry::Absent, None) => mounts.hides(&rel).map_or(Program::Other, Program::Unknown),
        (Entry::Unreadable(why), _) => Program::Unknown(why),
        (Entry::Symlink(_) | Entry::Dir | Entry::Other, _) => Program::Other,
    }
}

/// A regular file a command names: a script if its first bytes are `#!`,
/// read whole unless it is over [`MAX_READ_BYTES`]. A binary is never read
/// past its first two bytes.
fn read_script(root: &Path, rel: &str) -> Program {
    let mut file = match open_in_root(root, rel) {
        Ok(file) => file,
        Err(ReadErr::Absent) => return Program::Other,
        Err(ReadErr::Unreadable(why)) => return Program::Unknown(why),
    };
    let unreadable = |e: std::io::Error| Program::Unknown(format!("{rel}: {e}"));
    let mut text = Vec::new();
    if let Err(e) = (&mut file).take(2).read_to_end(&mut text) {
        return unreadable(e);
    }
    if text != b"#!" {
        return Program::Other;
    }
    text.clear();
    if let Err(e) = file
        .seek(SeekFrom::Start(0))
        .and_then(|_| file.take(MAX_READ_BYTES + 1).read_to_end(&mut text))
    {
        return unreadable(e);
    }
    // Bound first: `len as u64 < MAX` would not parse, which hides that
    // comparison from mutation testing.
    let read = text.len() as u64;
    if read > MAX_READ_BYTES {
        return Program::Unknown(format!("{rel}: larger than 64 KiB, not read"));
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
                Entry::File(_) | Entry::Other | Entry::Absent => continue,
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

/// The configuration btrbk would read by default ([`BtrbkConfig`]): the first
/// of [`BTRBK_CONFIGS`] that exists. When neither is here and one may be
/// there once that OS is up ([`Mounts::hides`]), it is unknown. It is `lstat`ed, never opened: beside
/// something that runs btrbk its presence is the whole signal, and parsing
/// another system's file would add risk and decide nothing. Anything there
/// that is not a regular file, or a symlink (not followed), is unknown:
/// btrbk would try to read it.
fn read_btrbk_config(root: &Path, mounts: &Mounts) -> BtrbkConfig {
    // One that may be there decides only when none surely is.
    let mut maybe = None;
    for rel in BTRBK_CONFIGS {
        match entry_at(root, rel) {
            Entry::Absent => maybe = maybe.or_else(|| mounts.hides(rel)),
            Entry::File(size_bytes) => {
                return BtrbkConfig::Present {
                    path: format!("/{rel}"),
                    size_bytes,
                };
            }
            Entry::Dir | Entry::Other => {
                return BtrbkConfig::Unreadable {
                    reason: format!("{rel}: not a regular file"),
                };
            }
            Entry::Symlink(reason) | Entry::Unreadable(reason) => {
                return BtrbkConfig::Unreadable { reason };
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
/// written, and the flag says so: only that OS's systemd can resolve it.
fn expand_specifiers(unit: &str, raw: &str) -> (String, bool) {
    let stem = stem_of(unit);
    let (prefix, instance) = stem.split_once('@').unwrap_or((stem, ""));
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
            Some('i') => out.push_str(instance),
            Some('n') => out.push_str(unit),
            Some('N') => out.push_str(stem),
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
enum UnitFile {
    Text(String),
    NotFound,
    /// A symlink in the vendor tree: a package's alias.
    VendorAlias,
    Unknown(String),
}

/// `name`'s unit file, by systemd's precedence: the first tree that has it;
/// an instance with no file of its own uses its template's. A symlink is not
/// followed: in the vendor tree it is a package's alias and is skipped, in
/// the others it is `systemctl link`, `mask` or an `enable` alias — told
/// apart only by reading the link, which would update its access time — so
/// it is unknown.
fn find_unit_file(root: &Path, trees: &[Tree], name: &str) -> UnitFile {
    for candidate in std::iter::once(name.to_string()).chain(template_of(name)) {
        for tree in trees {
            match &tree.listing {
                Listing::Absent => continue,
                Listing::Unreadable(why) => return UnitFile::Unknown(why.clone()),
                Listing::Names(names) if !names.contains(&candidate) => continue,
                Listing::Names(_) => {}
            }
            let rel = format!("{}/{candidate}", tree.rel);
            match read_file(root, &rel) {
                FileRead::Text(text) => return UnitFile::Text(text),
                FileRead::Absent => return UnitFile::Unknown(format!("{rel}: listed, then gone")),
                FileRead::Symlink(_) if tree.rel == VENDOR_TREE => return UnitFile::VendorAlias,
                FileRead::Symlink(why) | FileRead::Unknown(why) => return UnitFile::Unknown(why),
                FileRead::NotFile => {
                    return UnitFile::Unknown(format!("{rel}: not a regular file"));
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

/// The texts of `name`'s drop-ins in the order systemd applies them: each
/// file name once — the most specific directory, then the highest tree, wins
/// a name, so the first met in that order — sorted by file name across every
/// directory.
fn read_drop_ins(root: &Path, trees: &[Tree], name: &str) -> Result<Vec<String>, String> {
    let mut chosen: BTreeMap<String, (usize, String)> = BTreeMap::new();
    for dir in drop_in_dirs(name) {
        for (rank, tree) in trees.iter().enumerate() {
            match &tree.listing {
                Listing::Absent => continue,
                Listing::Unreadable(why) => return Err(why.clone()),
                Listing::Names(names) if !names.contains(&dir) => continue,
                Listing::Names(_) => {}
            }
            let rel = format!("{}/{dir}", tree.rel);
            let gone = || format!("{rel}: listed, then gone");
            match entry_at(root, &rel) {
                Entry::Dir => {}
                Entry::Symlink(_) if tree.rel == VENDOR_TREE => continue,
                Entry::Symlink(why) | Entry::Unreadable(why) => return Err(why),
                // A file with a drop-in directory's name holds no drop-ins.
                Entry::File(_) | Entry::Other => continue,
                Entry::Absent => return Err(gone()),
            }
            let files = match list_dir(root, &rel) {
                Listing::Names(files) => files,
                Listing::Absent => return Err(gone()),
                Listing::Unreadable(why) => return Err(why),
            };
            for file in files.into_iter().filter(|f| f.ends_with(".conf")) {
                let path = format!("{rel}/{file}");
                chosen.entry(file).or_insert((rank, path));
            }
        }
    }
    let mut texts = Vec::new();
    for (_, (rank, rel)) in chosen {
        match read_file(root, &rel) {
            FileRead::Text(text) => texts.push(text),
            FileRead::NotFile => {}
            FileRead::Symlink(_) if trees[rank].rel == VENDOR_TREE => {}
            FileRead::Absent => return Err(format!("{rel}: listed, then gone")),
            FileRead::Symlink(why) | FileRead::Unknown(why) => return Err(why),
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
    VendorAlias,
    Unknown(String),
}

fn load_unit(root: &Path, trees: &[Tree], name: &str) -> Loaded {
    let main = match find_unit_file(root, trees, name) {
        UnitFile::Text(text) => text,
        UnitFile::NotFound => return Loaded::NotFound,
        UnitFile::VendorAlias => return Loaded::VendorAlias,
        UnitFile::Unknown(why) => return Loaded::Unknown(why),
    };
    match read_drop_ins(root, trees, name) {
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

/// [`words`], with a quoted script that names btrbk — `sh -c 'btrbk run'`
/// — split into its own words in place.
fn command_words(line: &str) -> Vec<String> {
    words(line)
        .into_iter()
        .flat_map(|word| {
            let inner = words(&word);
            if inner.iter().any(|w| naming(w) != Naming::Not) {
                inner
            } else {
                vec![word]
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
/// an argument is btrbk's name too.
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
    /// relative path), or a short-option cluster this does not unpick.
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

/// What a command or script line runs that is btrbk's: each btrbk
/// invocation with the `-c FILE`, `-cFILE`, `--config FILE` or
/// `--config=FILE` after it — its arguments end at an operator or a comment,
/// and are not looked at again — and each word only like btrbk's name.
fn btrbk_invocations(line: &str) -> Vec<Found> {
    let ends = |w: &String| OPERATORS.contains(&w.as_str()) || w.starts_with('#');
    let mut words = command_words(line).into_iter().peekable();
    let mut found = Vec::new();
    while let Some(word) = words.next() {
        match naming(&word) {
            Naming::Btrbk => {}
            Naming::Like => {
                found.push(Found::Like(word));
                continue;
            }
            Naming::Not => continue,
        }
        let mut arg = ConfigArg::Default;
        while let Some(w) = words.next_if(|w| !ends(w)) {
            if w == "-c" || w == "--config" {
                arg = words
                    .next_if(|p| !ends(p))
                    .map_or_else(|| ConfigArg::Unresolved(w.clone()), |p| config_arg(&p));
            } else if let Some(path) = w.strip_prefix("--config=") {
                arg = config_arg(path);
            } else if let Some(path) = w.strip_prefix("-c") {
                arg = config_arg(path);
            } else if w.starts_with('-')
                && w.contains('c')
                && w[1..].chars().all(|c| c.is_ascii_alphabetic())
            {
                arg = ConfigArg::Unresolved(w.clone());
            }
        }
        found.push(Found::Btrbk(arg));
    }
    found
}

/// The absolute paths a command line names that could be a script it runs:
/// every word, and every word of a quoted script, that starts with `/` once
/// systemd's prefixes are off — each once. A word with a variable, a glob or
/// a specifier left unresolved names what only that OS could resolve, and
/// is not read; one with btrbk's name is [`btrbk_invocations`]'s.
fn named_paths(line: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    for word in words(line) {
        let inner = words(&word);
        candidates.push(word);
        candidates.extend(inner);
    }
    let mut paths: Vec<String> = Vec::new();
    for candidate in candidates {
        let path = candidate.trim_start_matches(['@', '-', ':', '+', '!']);
        if path.starts_with('/')
            && naming(path) == Naming::Not
            && !path.contains(['$', '%', '*', '?', '[', '~', '`'])
            && !paths.iter().any(|p| p == path)
        {
            paths.push(path.to_string());
        }
    }
    paths
}

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
fn path_outcome(root: &Path, mounts: &Mounts, path: &str) -> Outcome {
    let rel = path.trim_start_matches('/');
    match entry_at(root, rel) {
        Entry::File(_) => Outcome::Present(path.to_string()),
        Entry::Absent => mounts.hides(rel).map_or_else(
            || Outcome::Absent(format!("{path} is absent")),
            Outcome::Unknown,
        ),
        Entry::Dir | Entry::Other => Outcome::Unknown(format!("{rel}: not a regular file")),
        Entry::Symlink(why) | Entry::Unreadable(why) => Outcome::Unknown(why),
    }
}

/// When a unit enabled by `dirs` runs: at every boot, unless every target
/// that pulls it in is one a system reaches only to stop, sleep or start from
/// its initrd.
fn boot_when(dirs: &[String]) -> String {
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
        Some(first) if !at_boot => format!("when {first} starts"),
        _ => "at every boot".to_string(),
    }
}

/// When a timer's service runs. systemd.timer(5): with `Persistent=true` and
/// `OnCalendar=`, the last trigger is stored on disk
/// (`/var/lib/systemd/timers/stamp-<timer>`) and a timer activated at boot
/// triggers at once if it would have triggered while the system was off. The
/// man page does not say what happens with no stamp yet; this reads it as
/// nothing to catch up from, so the next calendar time — an inference, not a
/// documented guarantee.
fn timer_when(root: &Path, timer: &str, settings: &UnitSettings) -> String {
    if settings.on_boot {
        return "soon after boot (OnBootSec=, OnStartupSec= or OnActiveSec=)".to_string();
    }
    if settings.persistent && settings.on_calendar {
        return match entry_at(root, &format!("var/lib/systemd/timers/stamp-{timer}")) {
            Entry::File(_) => "straight after boot (Persistent catch-up)".to_string(),
            Entry::Absent => "at its next scheduled time after boot".to_string(),
            _ => "straight after boot or at its next scheduled time (its stamp is unknown)"
                .to_string(),
        };
    }
    "at its next scheduled time after boot".to_string()
}

/// A unit the walk reaches: what starts it, when it runs, and whether a name
/// on the way is btrbk's.
struct Pull {
    name: String,
    via: Option<String>,
    when: String,
    named: bool,
}

/// What a unit's commands run that is btrbk: each invocation's config, and
/// the script it is in when its command does not run btrbk itself.
type Runs = Vec<(Option<String>, ConfigArg)>;

/// Everything found about btrbk in one OS.
struct Findings<'a> {
    root: &'a Path,
    trees: &'a [Tree],
    mounts: &'a Mounts,
    default_config: &'a BtrbkConfig,
    runners: Vec<(BtrbkRunner, Outcome)>,
    unknowns: Vec<String>,
    problems: Vec<String>,
}

impl Findings<'_> {
    /// Something that could not be told, and the read behind it, if any.
    fn unknown(&mut self, reason: String, problem: Option<String>) {
        self.unknowns.push(reason);
        self.problems.extend(problem);
    }

    /// From the enabled units, everything they start: each unit read once
    /// ([`Findings::unit`]), at most [`MAX_UNITS`], and each way it is
    /// started reported with what its commands run.
    fn walk(&mut self, enabled: &[EnabledUnit]) {
        let mut queue: VecDeque<Pull> = enabled
            .iter()
            .map(|u| Pull {
                name: u.name.clone(),
                via: None,
                when: boot_when(&u.dirs),
                named: u.name.contains(BTRBK),
            })
            .collect();
        let mut read: BTreeMap<String, Runs> = BTreeMap::new();
        while let Some(pull) = queue.pop_front() {
            if !read.contains_key(&pull.name) {
                if read.len() == MAX_UNITS {
                    self.unknowns.push(format!(
                        "more than {MAX_UNITS} units start at boot: the rest were not read"
                    ));
                    break;
                }
                let (runs, next) = self.unit(&pull);
                read.insert(pull.name.clone(), runs);
                queue.extend(next);
            }
            for (script, arg) in read[&pull.name].clone() {
                self.runner(&pull.name, pull.via.as_deref(), script, &pull.when, arg);
            }
        }
        if let Some(daemon) = CRON_UNITS.iter().find(|unit| read.contains_key(**unit)) {
            self.cron(daemon);
        }
    }

    /// One unit, on its first reach: what its commands run, and the units it
    /// starts — what a timer, path or socket unit starts, then each unit its
    /// `[Unit]` section pulls in.
    fn unit(&mut self, pull: &Pull) -> (Runs, Vec<Pull>) {
        let name = pull.name.as_str();
        let who = match &pull.via {
            Some(via) => format!("{via} starts {name}, which"),
            None => name.to_string(),
        };
        let settings = match load_unit(self.root, self.trees, name) {
            Loaded::Settings(settings) => settings,
            Loaded::NotFound => {
                self.named_only(&who, pull.named, "was not found");
                return (Vec::new(), Vec::new());
            }
            Loaded::VendorAlias => {
                self.named_only(&who, pull.named, "is a package's alias, not followed");
                return (Vec::new(), Vec::new());
            }
            Loaded::Unknown(why) => {
                self.unknown(format!("{who} may run btrbk: {why}"), Some(why));
                return (Vec::new(), Vec::new());
            }
        };
        let kind = unit_type(name);
        if pull.named && !STARTERS.contains(&kind) {
            self.unknowns.push(format!("{who} is named for btrbk"));
        }
        let mut runs = Vec::new();
        for command in settings.exec.values().flatten() {
            // systemd resolves a command's specifiers before it runs it.
            let (command, _) = expand_specifiers(name, command);
            runs.extend(self.command(&who, &command));
        }
        let mut next = Vec::new();
        let starts = match kind {
            "timer" => Some((
                settings.unit.clone(),
                timer_when(self.root, name, &settings),
            )),
            "path" => Some((
                settings.unit.clone(),
                "when its path condition is met".into(),
            )),
            "socket" if settings.accept => Some((
                // A service instance per connection: only its template is known.
                Some(format!("{}@.service", stem_of(name))),
                "when something connects to it".into(),
            )),
            "socket" => Some((
                settings.service.clone(),
                "when something connects to it".into(),
            )),
            _ => None,
        };
        if let Some((target, when)) = starts {
            let target = match target {
                Some(target) if !settings.accept => resolve_name(name, &target),
                Some(target) => Some(target),
                None => Some(sibling(name, "service")),
            };
            match target {
                Some(target) => next.push(Pull {
                    named: pull.named || target.contains(BTRBK),
                    name: target,
                    via: Some(name.to_string()),
                    when,
                }),
                None => self.unknowns.push(format!(
                    "{who} starts a unit only that OS's systemd can name"
                )),
            }
        }
        for (key, raw) in &settings.pulls {
            let Some(target) = resolve_name(name, raw) else {
                self.unknowns.push(format!(
                    "{who} pulls in {raw}, which only that OS's systemd can resolve"
                ));
                continue;
            };
            let when = match key.as_str() {
                "OnFailure" => format!("if {name} fails"),
                "OnSuccess" => format!("after {name} succeeds"),
                _ => pull.when.clone(),
            };
            next.push(Pull {
                named: target.contains(BTRBK),
                name: target,
                via: Some(name.to_string()),
                when,
            });
        }
        (runs, next)
    }

    /// A unit whose file says nothing: unknown only when a name on the way is
    /// btrbk's.
    fn named_only(&mut self, who: &str, named: bool, what: &str) {
        if named {
            self.unknowns.push(format!(
                "{who} is named for btrbk, and its unit file {what}"
            ));
        }
    }

    /// What one command line runs that is btrbk: in it, or in a script it
    /// names by an absolute path ([`named_paths`]), read one level deep. A
    /// word only like btrbk's name, and a script that could not be read, are
    /// unknown, said as `who`.
    fn command(&mut self, who: &str, line: &str) -> Runs {
        let mut runs = self.line(who, None, line);
        for path in named_paths(line) {
            match program_at(self.root, self.mounts, &path) {
                Program::Script(text) => {
                    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
                        runs.extend(self.line(who, Some(&path), line));
                    }
                }
                Program::Other => {}
                Program::Unknown(why) => self.unknown(
                    format!("{who} may run btrbk through {path}: {why}"),
                    Some(why),
                ),
            }
        }
        runs
    }

    /// btrbk's invocations in one line — of `script`, or of the command
    /// itself.
    fn line(&mut self, who: &str, script: Option<&str>, line: &str) -> Runs {
        let mut runs = Vec::new();
        for found in btrbk_invocations(line) {
            match found {
                Found::Btrbk(arg) => runs.push((script.map(String::from), arg)),
                Found::Like(word) => {
                    let what = match script {
                        Some(script) => format!("{script}, which runs {word}"),
                        None => word,
                    };
                    self.unknowns
                        .push(format!("{who} runs {what}, named for btrbk"));
                }
            }
        }
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
                let outcome = path_outcome(self.root, self.mounts, &path);
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
    /// up on what the system missed while it was off.
    fn cron(&mut self, daemon: &str) {
        let mut files: Vec<String> = CRON_FILES.iter().map(|f| f.to_string()).collect();
        for dir in CRON_DIRS {
            match entry_at(self.root, dir) {
                Entry::Dir => match list_dir(self.root, dir) {
                    Listing::Names(names) => {
                        files.extend(names.into_iter().map(|n| format!("{dir}/{n}")));
                    }
                    Listing::Absent => {}
                    Listing::Unreadable(why) => {
                        self.unknown(format!("cron ({daemon}) is enabled and {why}"), Some(why))
                    }
                },
                Entry::Symlink(why) | Entry::Unreadable(why) => {
                    self.unknown(format!("cron ({daemon}) is enabled and {why}"), Some(why));
                }
                Entry::File(_) | Entry::Other | Entry::Absent => {}
            }
        }
        let mut texts = Vec::new();
        for rel in files {
            match read_file(self.root, &rel) {
                FileRead::Text(text) => texts.push((rel, text)),
                FileRead::Absent | FileRead::NotFile => {}
                FileRead::Symlink(why) | FileRead::Unknown(why) => {
                    self.unknown(format!("cron ({daemon}) is enabled and {why}"), Some(why));
                }
            }
        }
        let uncommented = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .map(String::from)
                .collect()
        };
        let anacron = texts
            .iter()
            .find(|(rel, _)| rel == ANACRONTAB)
            .map_or_else(Vec::new, |(_, text)| uncommented(text));
        for (rel, text) in &texts {
            let catch_up = rel == ANACRONTAB
                || ANACRON_DIRS.iter().any(|dir| {
                    rel.starts_with(&format!("etc/{dir}/"))
                        && anacron.iter().any(|l| l.contains(dir))
                });
            let who = format!("{rel} (cron, {daemon})");
            for line in uncommented(text) {
                let when = if catch_up {
                    "soon after boot (anacron catch-up)"
                } else if line.split_whitespace().next() == Some("@reboot") {
                    "at boot (cron @reboot)"
                } else {
                    "on its cron schedule"
                };
                for (script, arg) in self.command(&who, &line) {
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

/// What [`read`] found in one OS root.
pub(super) struct Reading {
    pub units: EnabledUnits,
    pub config: BtrbkConfig,
    pub at_boot: BtrbkAtBoot,
    /// What could not be read, for the drive's `problems`, each once.
    pub problems: Vec<String>,
}

/// Read what this OS starts at boot and whether any of it runs btrbk.
pub(super) fn read(root: &Path) -> Reading {
    let trees: Vec<Tree> = UNIT_TREES
        .iter()
        .map(|rel| Tree {
            rel,
            listing: list_dir(root, rel),
        })
        .collect();
    let units = read_enabled_units(root, &trees);
    let mounts = read_mounts(root);
    let config = read_btrbk_config(root, &mounts);
    let mut findings = Findings {
        root,
        trees: &trees,
        mounts: &mounts,
        default_config: &config,
        runners: Vec::new(),
        unknowns: Vec::new(),
        problems: Vec::new(),
    };
    let mut problems = Vec::new();
    if let Mounts::Unknown(why) = &mounts {
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
    use crate::recovery_os::testutil;
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
    const UNMOUNTED: Mounts = Mounts::Points(Vec::new());
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
        // A config that is a symlink is not followed: unknown.
        exec("/usr/bin/btrbk -c /opt/x.conf run");
        fs::remove_file(root.join("opt/x.conf")).unwrap();
        link(root, "opt/x.conf", "/etc/hostname");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(b.runners[0].config_present, None);
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
    fn a_linked_or_masked_unit_may_run_btrbk_and_a_vendor_alias_does_not() {
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
        let why = format!(
            "etc/systemd/system/foo.service: {} is a symlink, not followed",
            masked.display()
        );
        let r = read(root);
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
        assert_eq!(
            r.at_boot.reasons,
            [format!("foo.service may run btrbk: {why}")]
        );
        assert_eq!(r.problems, [why]);
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
        let r = read(root);
        assert_eq!(r.at_boot, nothing());
        assert!(r.problems.is_empty(), "{:?}", r.problems);
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
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [
                "btrbk-alias.service is named for btrbk, and its unit file is a package's alias, not followed"
            ]
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
        // A masked drop-in (a link) cannot be read: unknown.
        fs::remove_file(root.join("etc/systemd/system/service.d/99-snap.conf")).unwrap();
        link(
            root,
            "etc/systemd/system/job.service.d/40-mask.conf",
            "/dev/null",
        );
        assert_eq!(at_boot(root).verdict, BootVerdict::May);
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
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [format!(
                "cron (cronie.service) is enabled and etc/crontab: {} is a symlink, not followed",
                crontab.display()
            )]
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
        assert_eq!(read_btrbk_config(root, &UNMOUNTED), BtrbkConfig::Absent);
        write(root, second, CONF);
        let at_second = BtrbkConfig::Present {
            path: "/etc/btrbk/btrbk.conf".into(),
            size_bytes: CONF.len() as u64,
        };
        assert_eq!(read_btrbk_config(root, &UNMOUNTED), at_second);
        // btrbk 0.32 takes /etc/btrbk.conf when it exists, whatever else does.
        write(root, first, "volume /a\n");
        let at_first = BtrbkConfig::Present {
            path: "/etc/btrbk.conf".into(),
            size_bytes: 10,
        };
        assert_eq!(read_btrbk_config(root, &UNMOUNTED), at_first);
        fs::remove_file(root.join(second)).unwrap();
        assert_eq!(read_btrbk_config(root, &UNMOUNTED), at_first);
        write(root, first, "");
        assert_eq!(
            read_btrbk_config(root, &UNMOUNTED),
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
        assert_eq!(read_btrbk_config(root, &UNMOUNTED), not_file);
        assert!(
            read(root)
                .problems
                .contains(&"etc/btrbk.conf: not a regular file".to_string())
        );
        fs::remove_dir(root.join(first)).unwrap();
        fs::remove_file(root.join(second)).unwrap();
        fs::create_dir(root.join(second)).unwrap();
        assert_eq!(
            read_btrbk_config(root, &UNMOUNTED),
            BtrbkConfig::Unreadable {
                reason: "etc/btrbk/btrbk.conf: not a regular file".into()
            }
        );
        // A link is not followed.
        fs::remove_dir(root.join(second)).unwrap();
        link(root, second, "/etc/hostname");
        assert_eq!(
            read_btrbk_config(root, &UNMOUNTED),
            BtrbkConfig::Unreadable {
                reason: format!(
                    "etc/btrbk/btrbk.conf: {} is a symlink, not followed",
                    root.join(second).display()
                )
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
    fn a_link_among_the_drop_ins_is_skipped_for_the_vendor_and_unknown_for_etc() {
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
        assert_eq!(at_boot(root).verdict, BootVerdict::Will, "skipped");
        // An administrator's link: a directory of drop-ins, unknown.
        link(root, "etc/systemd/system/btrbk.service.d", "/etc/elsewhere");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [format!(
                "btrbk.service may run btrbk: etc/systemd/system/btrbk.service.d: {} is a \
                 symlink, not followed",
                root.join("etc/systemd/system/btrbk.service.d").display()
            )]
        );
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
        let why = format!(
            "etc/systemd/system/service.d/50-mask.conf: {} is a symlink, not followed",
            root.join("etc/systemd/system/service.d/50-mask.conf")
                .display()
        );
        let r = read(root);
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
            assert_eq!(btrbk_invocations(line), want, "{line:?}");
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
    fn the_paths_a_command_names_are_its_absolute_words() {
        let cases: Vec<(&str, Vec<&str>)> = vec![
            (
                "/usr/bin/nice -n 19 /usr/local/bin/b.sh",
                vec!["/usr/bin/nice", "/usr/local/bin/b.sh"],
            ),
            // systemd's prefixes come off; a quoted script's words count.
            ("-@/usr/bin/x arg0 /a", vec!["/usr/bin/x", "/a"]),
            (
                "/bin/sh -c 'cd /srv && /opt/r.sh'",
                vec!["/bin/sh", "/srv", "/opt/r.sh"],
            ),
            // Each once.
            ("/a /a '/a'", vec!["/a"]),
            // Only what the text names: no variable, glob, or specifier left
            // unresolved — in a cron line `%` is cron's newline.
            ("/x/$V /x/${V} /x/* /x/? /x/[ab] /x/~ /opt/%i.sh", vec![]),
            ("'/x/`y`'", vec!["/x/"]),
            // btrbk's name is btrbk_invocations'.
            ("/usr/bin/btrbk /opt/btrbk.sh /etc/btrbk", vec![]),
            ("run x.sh ./y.sh", vec![]),
        ];
        for (line, want) in cases {
            assert_eq!(named_paths(line), want, "{line:?}");
        }
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
            // A template itself (an Accept=yes socket's service) has no instance.
            ("job@.service", "snap@%i.service", Some("snap@job.service")),
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
            "at every boot"
        );
        assert_eq!(
            boot_when(&dirs(&["usr/lib/systemd/system/initrd.target.requires"])),
            "when initrd.target starts"
        );
        assert_eq!(
            boot_when(&dirs(&[
                "etc/systemd/system/suspend.target.wants",
                "etc/systemd/system/sysinit.target.upholds"
            ])),
            "at every boot"
        );
        assert_eq!(boot_when(&[]), "at every boot");
        // An unknown stamp says both.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("var/lib/systemd/timers/stamp-x.timer")).unwrap();
        let s = UnitSettings {
            persistent: true,
            on_calendar: true,
            ..UnitSettings::default()
        };
        assert_eq!(
            timer_when(dir.path(), "x.timer", &s),
            "straight after boot or at its next scheduled time (its stamp is unknown)"
        );
        // Persistent alone, without OnCalendar=, catches nothing up.
        write(dir.path(), "var/lib/systemd/timers/stamp-y.timer", "");
        let s = UnitSettings {
            persistent: true,
            ..UnitSettings::default()
        };
        assert_eq!(
            timer_when(dir.path(), "y.timer", &s),
            "at its next scheduled time after boot"
        );
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
    fn a_script_a_unit_runs_is_read_one_level_deep() {
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
        // One level only: a script the script runs is not read (declared).
        script(root, "usr/local/bin/backup.sh", "/usr/local/bin/inner.sh\n");
        script(root, "usr/local/bin/inner.sh", "btrbk run\n");
        assert_eq!(at_boot(root), nothing(), "one level");
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
        // A path only that OS could resolve is not read: a variable, a glob,
        // a specifier that is not the unit's own.
        let odd = "usr/local/bin/$X.sh";
        script(root, odd, "btrbk run\n");
        for command in [
            "/usr/local/bin/$X.sh",
            "/usr/local/bin/*.sh",
            "/usr/local/bin/%H.sh",
        ] {
            wrapped(root, command);
            assert_eq!(at_boot(root), nothing(), "{command}");
        }
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
        // A script named through a link: a program link, not read (declared).
        fs::remove_file(root.join("usr/local/bin/backup.sh")).unwrap();
        script(root, "usr/local/lib/real.sh", "btrbk run\n");
        link(root, "usr/local/bin/backup.sh", "/usr/local/lib/real.sh");
        assert_eq!(at_boot(root), nothing(), "a link is not read");
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
            // Nothing where Arch's link leads: on another layout it may lead
            // elsewhere, so may, never no.
            fs::remove_file(root.join(rel)).unwrap();
            assert_eq!(
                at_boot(root).reasons,
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
        // A link this does not know, on the way: may.
        link(root, "opt", "/srv/opt");
        wrapped(root, "/opt/tools/backup.sh");
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [format!(
                "backup.service may run btrbk through /opt/tools/backup.sh: opt/tools: {} is a \
                 symlink, not followed",
                root.join("opt").display()
            )]
        );
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
        unit(
            root,
            ETC,
            "big.target",
            &format!("[Unit]\nWants={}\n", names(4095)),
        );
        assert_eq!(at_boot(root), nothing(), "4096 units");
        unit(
            root,
            ETC,
            "big.target",
            &format!("[Unit]\nWants={}\n", names(4096)),
        );
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            ["more than 4096 units start at boot: the rest were not read"]
        );
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
        // What is here under a mount point is read as it is.
        script(root, "root/bin/backup.sh", "btrbk run\n");
        wrapped(root, "/root/bin/backup.sh");
        assert_eq!(at_boot(root).verdict, BootVerdict::Will);
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
        let r = read(root);
        let why = format!(
            "etc/fstab: {} is a symlink, not followed",
            root.join("etc/fstab").display()
        );
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
            read(root).config,
            BtrbkConfig::Unreadable {
                reason: format!(
                    "etc/btrbk.conf: not here, and what that OS mounts over its root is \
                     unknown: {why}"
                )
            }
        );
        // Not a regular file: unknown the same way.
        fs::remove_file(root.join("etc/fstab")).unwrap();
        fs::create_dir(root.join("etc/fstab")).unwrap();
        let r = read(root);
        assert_eq!(r.problems[0], "etc/fstab: not a regular file");
        assert_eq!(r.at_boot.verdict, BootVerdict::May);
    }
}
