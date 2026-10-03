//! Whether booting a recovery OS would run btrbk (bd DAS-Backup-Manager-1yg)
//! — on bare metal or in the update VM — and when.
//!
//! systemd starts at boot what the dependency directories of its unit trees
//! name: `<unit>.wants/`, `.requires/` and `.upholds/` under
//! `etc/systemd/system`, where `systemctl enable` puts its links, and under
//! `usr/local/lib/systemd/system` and `usr/lib/systemd/system`, where
//! packages enable their own. Each entry is a symlink to a unit file; only
//! the names are read. For each enabled service or socket, and for the
//! service each enabled timer or path unit starts, the unit file is looked up
//! by name in systemd's precedence order — an instance falls back to its
//! template — and read with its drop-ins; a unit runs btrbk if any `Exec…=`
//! command left after the drop-ins names it (`btrbk`, a path ending in it,
//! or a `sh -c` script that does). When a cron daemon is enabled, its
//! tables and scripts are read the same way. Each runner's config is the
//! `-c` it passes, else btrbk's default; it is only `lstat`ed.
//!
//! Everything is read with the parent module's rules: `O_NOATIME`, no
//! symlink followed, absent told from unreadable, and nothing written.
//! Reading a link would update its access time, so a link is never read,
//! which means:
//!
//! - a unit file linked into `/etc` by `systemctl link`, and one masked
//!   there by `systemctl mask` (a link to `/dev/null`), cannot be told
//!   apart: both count as "may run btrbk";
//! - a symlink among the vendor units is a package's alias (Arch ships
//!   `dbus.service` → `dbus-broker.service`) and is skipped, unless its name
//!   is btrbk's.
//!
//! Not read: the scripts a command or a cron job runs, units generators
//! create at boot, user units, and `/etc/rc.local`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use super::{
    BTRBK_CONFIGS, BootVerdict, BtrbkAtBoot, BtrbkConfig, BtrbkRunner, EnabledUnit, EnabledUnits,
    ReadErr, dir_names_noatime, io_error, read_in_root, resolve_in_root,
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
const CRON_FILES: [&str; 2] = ["etc/crontab", "etc/anacrontab"];
const CRON_DIRS: [&str; 7] = [
    "etc/cron.d",
    "etc/cron.hourly",
    "etc/cron.daily",
    "etc/cron.weekly",
    "etc/cron.monthly",
    "var/spool/cron",
    "var/spool/cron/crontabs",
];
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
/// of [`BTRBK_CONFIGS`] that exists. It is `lstat`ed, never opened: beside
/// something that runs btrbk its presence is the whole signal, and parsing
/// another system's file would add risk and decide nothing. Anything there
/// that is not a regular file, or a symlink (not followed), is unknown:
/// btrbk would try to read it.
fn read_btrbk_config(root: &Path) -> BtrbkConfig {
    for rel in BTRBK_CONFIGS {
        match entry_at(root, rel) {
            Entry::Absent => {}
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
    BtrbkConfig::Absent
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

/// The unit of the same name and another type: what a timer or path unit
/// starts when it names no `Unit=`.
fn sibling(name: &str, kind: &str) -> String {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    format!("{stem}.{kind}")
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
/// the others it is `systemctl link` or `mask` — told apart only by reading
/// the link, which would update its access time — so it is unknown.
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
    /// `[Timer]`/`[Path]` `Unit=`.
    unit: Option<String>,
    persistent: bool,
    on_calendar: bool,
    on_boot: bool,
}

/// systemd's boolean: `1`, `yes`, `y`, `true`, `t`, `on`.
fn parse_bool(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "yes" | "y" | "true" | "t" | "on"
    )
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
                ("Timer" | "Path", "Unit") => {
                    settings.unit = (!value.is_empty()).then(|| value.to_string());
                }
                ("Timer", "Persistent") => settings.persistent = parse_bool(value),
                ("Timer", "OnCalendar") => settings.on_calendar = !value.is_empty(),
                ("Timer", "OnBootSec" | "OnStartupSec") => settings.on_boot = !value.is_empty(),
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
/// a backslash escapes the next character.
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

/// [`words`], with a quoted script that runs btrbk — `sh -c 'btrbk run'` —
/// split into its own words in place.
fn command_words(line: &str) -> Vec<String> {
    words(line)
        .into_iter()
        .flat_map(|word| {
            let inner = words(&word);
            if inner.iter().any(|w| names_btrbk(w)) {
                inner
            } else {
                vec![word]
            }
        })
        .collect()
}

/// A word naming btrbk: `btrbk`, or a path ending in it, after systemd's
/// command prefixes (`-`, `@`, `:`, `+`, `!`). Any such word counts, the
/// cautious reading: a path to `/etc/btrbk` passed as an argument does too.
fn names_btrbk(word: &str) -> bool {
    let word = word.trim_start_matches(['@', '-', ':', '+', '!']);
    word.rsplit('/').next() == Some(BTRBK)
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

/// One entry per btrbk invocation in a command or script line, with the
/// `-c FILE`, `-cFILE`, `--config FILE` or `--config=FILE` after it. An
/// operator or a comment ends its arguments.
fn btrbk_invocations(line: &str) -> Vec<ConfigArg> {
    let mut words = command_words(line).into_iter();
    let mut found = Vec::new();
    while let Some(word) = words.next() {
        if !names_btrbk(&word) {
            continue;
        }
        let mut arg = ConfigArg::Default;
        let mut rest = words.clone();
        while let Some(w) = rest.next() {
            if OPERATORS.contains(&w.as_str()) || w.starts_with('#') {
                break;
            }
            if w == "-c" || w == "--config" {
                arg = rest
                    .next()
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
        found.push(arg);
    }
    found
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
fn path_outcome(root: &Path, path: &str) -> Outcome {
    let rel = path.trim_start_matches('/');
    match entry_at(root, rel) {
        Entry::File(_) => Outcome::Present(path.to_string()),
        Entry::Absent => Outcome::Absent(format!("{path} is absent")),
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
        return "soon after boot (OnBootSec=/OnStartupSec=)".to_string();
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

/// Everything found about btrbk in one OS.
struct Findings<'a> {
    root: &'a Path,
    trees: &'a [Tree],
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

    fn scan(&mut self, units: &[EnabledUnit]) {
        for unit in units {
            let name = unit.name.as_str();
            match unit_type(name) {
                "service" | "socket" => {
                    self.commands(name, None, &boot_when(&unit.dirs), name.contains(BTRBK));
                }
                "timer" | "path" => self.activator(name),
                _ if name.contains(BTRBK) => {
                    self.unknowns
                        .push(format!("{name} is enabled and named for btrbk"));
                }
                _ => {}
            }
        }
        if let Some(daemon) = units
            .iter()
            .map(|u| u.name.as_str())
            .find(|n| CRON_UNITS.contains(n))
        {
            self.cron(daemon);
        }
    }

    /// A timer or path unit: what it starts, and when.
    fn activator(&mut self, name: &str) {
        match load_unit(self.root, self.trees, name) {
            Loaded::Settings(settings) => {
                let target = settings
                    .unit
                    .clone()
                    .unwrap_or_else(|| sibling(name, "service"));
                let when = if unit_type(name) == "timer" {
                    timer_when(self.root, name, &settings)
                } else {
                    "when its path condition is met".to_string()
                };
                let named = name.contains(BTRBK) || target.contains(BTRBK);
                self.commands(&target, Some(name), &when, named);
            }
            Loaded::NotFound => self.named_only(name, name.contains(BTRBK), "was not found"),
            Loaded::VendorAlias => self.named_only(
                name,
                name.contains(BTRBK),
                "is a package's alias, not followed",
            ),
            Loaded::Unknown(why) => self.unknown(
                format!("{name} may start a unit that runs btrbk: {why}"),
                Some(why),
            ),
        }
    }

    /// A unit whose own commands run, started by `via` or enabled itself.
    fn commands(&mut self, name: &str, via: Option<&str>, when: &str, named: bool) {
        let who = match via {
            Some(via) => format!("{via} starts {name}, which"),
            None => name.to_string(),
        };
        match load_unit(self.root, self.trees, name) {
            Loaded::Settings(settings) => {
                let args: Vec<ConfigArg> = settings
                    .exec
                    .values()
                    .flatten()
                    .flat_map(|command| btrbk_invocations(command))
                    .collect();
                for arg in args {
                    self.runner(name.to_string(), via.map(String::from), when, arg);
                }
            }
            Loaded::NotFound => self.named_only(&who, named, "was not found"),
            Loaded::VendorAlias => {
                self.named_only(&who, named, "is a package's alias, not followed");
            }
            Loaded::Unknown(why) => {
                self.unknown(format!("{who} may run btrbk: {why}"), Some(why));
            }
        }
    }

    /// A unit whose file says nothing: unknown only when its name is btrbk's.
    fn named_only(&mut self, who: &str, named: bool, what: &str) {
        if named {
            self.unknowns.push(format!(
                "{who} is named for btrbk, and its unit file {what}"
            ));
        }
    }

    fn runner(&mut self, source: String, via: Option<String>, when: &str, arg: ConfigArg) {
        let (config, outcome) = match arg {
            ConfigArg::Default => (None, default_outcome(self.default_config)),
            ConfigArg::Path(path) => {
                let outcome = path_outcome(self.root, &path);
                (Some(path), outcome)
            }
            ConfigArg::Unresolved(raw) => {
                let why = format!("-c {raw} cannot be resolved without running that OS");
                (Some(raw), Outcome::Unknown(why))
            }
        };
        let runner = BtrbkRunner {
            source,
            via,
            when: when.to_string(),
            config,
            config_present: outcome.present(),
        };
        if !self.runners.iter().any(|(r, _)| *r == runner) {
            self.runners.push((runner, outcome));
        }
    }

    /// Cron's tables and scripts, read when a cron daemon is enabled.
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
        for rel in files {
            match read_file(self.root, &rel) {
                FileRead::Text(text) => {
                    let args: Vec<ConfigArg> = text
                        .lines()
                        .filter(|line| !line.trim_start().starts_with('#'))
                        .flat_map(btrbk_invocations)
                        .collect();
                    for arg in args {
                        self.runner(
                            rel.clone(),
                            Some(daemon.to_string()),
                            "on its cron schedule",
                            arg,
                        );
                    }
                }
                FileRead::Absent | FileRead::NotFile => {}
                FileRead::Symlink(why) | FileRead::Unknown(why) => {
                    self.unknown(format!("cron ({daemon}) is enabled and {why}"), Some(why));
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
/// (Persistent catch-up), with /etc/btrbk/btrbk.conf present`.
fn runner_text(runner: &BtrbkRunner, outcome: &Outcome) -> String {
    let what = match &runner.via {
        Some(daemon) if runner.source.contains('/') => {
            format!("{} (cron, {daemon}) runs btrbk", runner.source)
        }
        Some(via) => format!("{via} starts {}, which runs btrbk", runner.source),
        None => format!("{} runs btrbk", runner.source),
    };
    let config = match outcome {
        Outcome::Present(path) => format!("with {path} present"),
        Outcome::Absent(what) => format!("but {what}, so it stops at once"),
        Outcome::Unknown(why) => format!("its config unknown: {why}"),
    };
    format!("{what} {}, {config}", runner.when)
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
    let config = read_btrbk_config(root);
    let mut findings = Findings {
        root,
        trees: &trees,
        default_config: &config,
        runners: Vec::new(),
        unknowns: Vec::new(),
        problems: Vec::new(),
    };
    let mut problems = Vec::new();
    match &units {
        EnabledUnits::Listed { units } => findings.scan(units),
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
            when: when.into(),
            config: config.map(String::from),
            config_present: present,
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
                 (OnBootSec=/OnStartupSec=), {PRESENT}"
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
        let b = at_boot(root);
        assert_eq!(b.verdict, BootVerdict::May);
        assert_eq!(
            b.reasons,
            [
                "backup@daily.service runs btrbk at every boot, its config unknown: -c \
                 /etc/btrbk/%i.conf cannot be resolved without running that OS"
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
            ["btrbk.target is enabled and named for btrbk"]
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
        assert_eq!(read_btrbk_config(root), BtrbkConfig::Absent);
        write(root, second, CONF);
        let at_second = BtrbkConfig::Present {
            path: "/etc/btrbk/btrbk.conf".into(),
            size_bytes: CONF.len() as u64,
        };
        assert_eq!(read_btrbk_config(root), at_second);
        // btrbk 0.32 takes /etc/btrbk.conf when it exists, whatever else does.
        write(root, first, "volume /a\n");
        let at_first = BtrbkConfig::Present {
            path: "/etc/btrbk.conf".into(),
            size_bytes: 10,
        };
        assert_eq!(read_btrbk_config(root), at_first);
        fs::remove_file(root.join(second)).unwrap();
        assert_eq!(read_btrbk_config(root), at_first);
        write(root, first, "");
        assert_eq!(
            read_btrbk_config(root),
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
        assert_eq!(read_btrbk_config(root), not_file);
        assert!(
            read(root)
                .problems
                .contains(&"etc/btrbk.conf: not a regular file".to_string())
        );
        fs::remove_dir(root.join(first)).unwrap();
        fs::remove_file(root.join(second)).unwrap();
        fs::create_dir(root.join(second)).unwrap();
        assert_eq!(
            read_btrbk_config(root),
            BtrbkConfig::Unreadable {
                reason: "etc/btrbk/btrbk.conf: not a regular file".into()
            }
        );
        // A link is not followed.
        fs::remove_dir(root.join(second)).unwrap();
        link(root, second, "/etc/hostname");
        assert_eq!(
            read_btrbk_config(root),
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
        fs::create_dir_all(root.join(ETC)).unwrap();
        let fifo =
            std::ffi::CString::new(root.join(ETC).join("timers.target.wants").to_str().unwrap())
                .unwrap();
        // SAFETY: a valid NUL-terminated path; mkfifo only creates the node.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
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
        use ConfigArg::{Default, Path as P, Unresolved as U};
        let p = |s: &str| P(s.to_string());
        let u = |s: &str| U(s.to_string());
        let cases: Vec<(&str, Vec<ConfigArg>)> = vec![
            ("/usr/bin/btrbk run", vec![Default]),
            ("btrbk -q run", vec![Default]),
            ("-/usr/bin/btrbk run", vec![Default]),
            ("!!/usr/bin/btrbk run", vec![Default]),
            ("/usr/bin/nice -n 19 /usr/bin/btrbk run", vec![Default]),
            ("/usr/bin/ionice -c3 btrbk run", vec![Default]),
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
            ("btrbk run; foo -c /x", vec![Default]),
            ("btrbk run && bar -c /x", vec![Default]),
            ("btrbk run # -c /x", vec![Default]),
            ("btrbk run; btrbk -c /y run", vec![Default, p("/y")]),
            // A quoted script that is btrbk alone still runs it.
            ("/bin/sh -c '\"btrbk\"'", vec![Default]),
            ("btrbk clean", vec![Default]),
            ("btrbk -1c run", vec![Default]),
            ("btrbk -c /opt/my\\ x.conf run", vec![p("/opt/my x.conf")]),
            ("btrbk -c \"/opt/a\\\"b\" run", vec![p("/opt/a\"b")]),
            ("btrbk -c \"\" run", vec![u("")]),
            ("/usr/bin/btrbk-wrapper run", vec![]),
            ("/usr/bin/cat /etc/btrbk.conf", vec![]),
            ("echo btrbkx xbtrbk", vec![]),
            ("", vec![]),
            // Cautious: a path ending in btrbk counts, even as an argument.
            ("tar czf /tmp/x.tgz /etc/btrbk", vec![Default]),
        ];
        for (line, want) in cases {
            assert_eq!(btrbk_invocations(line), want, "{line:?}");
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
        // Empty assignments reset the timer settings too.
        let s = parse_unit([
            "[Timer]\nUnit=a.service\nUnit=\nOnCalendar=daily\nOnCalendar=\nOnBootSec=1\nOnBootSec=\n"
                .to_string(),
        ]);
        assert_eq!(s, UnitSettings::default());
        for (word, want) in [
            ("1", true),
            ("yes", true),
            ("Y", true),
            ("true", true),
            ("t", true),
            ("ON", true),
            ("0", false),
            ("no", false),
            ("false", false),
            ("off", false),
            ("", false),
            ("maybe", false),
        ] {
            assert_eq!(parse_bool(word), want, "{word:?}");
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
}
