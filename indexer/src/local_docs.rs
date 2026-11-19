//! The installation's own recovery documents (`[recovery_os].local_docs_dir`):
//! lists and tables in them that the configuration and the running system
//! decide, kept current (bd DAS-Backup-Manager-o77b, riwa.1).
//!
//! A document marks a list with two lines of their own:
//!
//! ```text
//! <!-- btrdasd:subvolumes source=<label> under=<path> -->
//! <!-- /btrdasd:subvolumes -->
//! ```
//!
//! and [`refresh_dir`] replaces whatever stands between them with the current,
//! non-retired subvolume entries of that source, sorted, comma-separated.
//! `under=` is optional: with it, only entries directly under that path (one
//! component below it, named by that component); without it, every entry, by
//! its full name. Everything outside a marked block is copied byte for byte.
//!
//! A second kind of block, `btrdasd:subvolume-table volume=<path>
//! [under=<path>] [level=top]`, holds a Markdown table of every entry of every
//! source on that volume, with its live mount point (bd
//! DAS-Backup-Manager-riwa.1): see [`render_table`]. A close marker names its
//! block's kind (`<!-- /btrdasd:subvolume-table -->`).
//!
//! Fails closed: a file whose markers do not pair (an open with no close, an
//! open inside another, a close with no open), name an unknown attribute or
//! source, or select no entry, is left exactly as it was and named. Markers
//! inside fenced code (```` ``` ```` or `~~~`) are text, never markers, so a
//! guide can show the syntax. Only regular `*.md` files directly in the
//! directory are read; a symlink is refused, never followed.

use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::btrbk_conf::{resolve_snapshot_names, source_subdir, source_targets};
use crate::config::Config;
use crate::fsutil::CommandRunner;

/// Every block marker opens with this; the word after it is the block's kind.
const PREFIX: &str = "<!-- btrdasd:";
const CLOSE_PREFIX: &str = "<!-- /btrdasd:";
#[cfg(test)]
const CLOSE: &str = "<!-- /btrdasd:subvolumes -->";
const END: &str = "-->";

/// What a block holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The names of a source's entries, comma-separated.
    List,
    /// A Markdown table of every entry on a volume (bd DAS-Backup-Manager-riwa.1).
    Table,
}

impl Kind {
    fn from_word(word: &str) -> Option<Self> {
        match word {
            "subvolumes" => Some(Kind::List),
            "subvolume-table" => Some(Kind::Table),
            _ => None,
        }
    }
    fn word(self) -> &'static str {
        match self {
            Kind::List => "subvolumes",
            Kind::Table => "subvolume-table",
        }
    }
    /// The attributes it takes, the required one first.
    fn attributes(self) -> &'static [&'static str] {
        match self {
            Kind::List => &["source", "under"],
            Kind::Table => &["volume", "under", "level"],
        }
    }
    fn close(self) -> String {
        format!("{CLOSE_PREFIX}{} -->", self.word())
    }
}

/// One btrfs mount, as `findmnt -P` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub target: String,
    pub uuid: String,
    /// The subvolume path mounted there, from the top of the filesystem.
    pub fsroot: String,
    pub options: String,
}

/// The live mounts, read at most once and only when a document has a table.
pub struct Mounts<'a> {
    probe: Box<dyn Fn() -> Result<Vec<Mount>, String> + 'a>,
    got: OnceCell<Result<Vec<Mount>, String>>,
}

impl<'a> Mounts<'a> {
    pub fn new(probe: impl Fn() -> Result<Vec<Mount>, String> + 'a) -> Self {
        Mounts {
            probe: Box::new(probe),
            got: OnceCell::new(),
        }
    }
    /// This host's mounts, through `runner`.
    pub fn probed(runner: &'a dyn CommandRunner) -> Self {
        Mounts::new(move || probe_mounts(runner))
    }
    fn get(&self) -> Result<&[Mount], String> {
        match self.got.get_or_init(|| (self.probe)()) {
            Ok(m) => Ok(m),
            Err(e) => Err(e.clone()),
        }
    }
}

/// Every btrfs mount on this host: `findmnt -P -t btrfs`. Read only.
pub fn probe_mounts(runner: &dyn CommandRunner) -> Result<Vec<Mount>, String> {
    let out = runner
        .output(Command::new("findmnt").args([
            "-P",
            "-t",
            "btrfs",
            "-o",
            "TARGET,UUID,FSROOT,OPTIONS",
        ]))
        .map_err(|e| format!("cannot run findmnt: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "findmnt exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_mounts(&String::from_utf8_lossy(&out.stdout))
}

/// `findmnt -P` output: one mount a line, `KEY="value"` pairs, a value's
/// awkward bytes written `\xHH`. A line missing a column is an error.
fn parse_mounts(text: &str) -> Result<Vec<Mount>, String> {
    let mut mounts = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut pairs: BTreeMap<&str, String> = BTreeMap::new();
        let mut rest = line.trim();
        while !rest.is_empty() {
            let (key, after) = rest
                .split_once("=\"")
                .ok_or_else(|| format!("findmnt line {line:?}: not KEY=\"value\" pairs"))?;
            let end = after
                .find('"')
                .ok_or_else(|| format!("findmnt line {line:?}: a value never closes"))?;
            pairs.insert(key.trim(), unescape(&after[..end]));
            rest = after[end + 1..].trim_start();
        }
        let mut take = |k: &str| {
            pairs
                .remove(k)
                .ok_or_else(|| format!("findmnt line {line:?}: no {k}"))
        };
        mounts.push(Mount {
            target: take("TARGET")?,
            uuid: take("UUID")?,
            fsroot: take("FSROOT")?,
            options: take("OPTIONS")?,
        });
    }
    Ok(mounts)
}

/// `\xHH` back to its byte.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && b.get(i + 1) == Some(&b'x')
            && let Some(v) = s
                .get(i + 2..i + 4)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What one document's refresh found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocOutcome {
    /// No marked block, or every block already current: not written.
    Current,
    /// Rewritten (or, checking only, would be).
    Updated,
    /// Left as it was, with why.
    Refused(String),
}

/// One document and what its refresh found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocReport {
    pub path: PathBuf,
    pub outcome: DocOutcome,
}

/// The current text for one block: `source`'s non-retired entries.
fn render(config: &Config, source: &str, under: Option<&str>) -> Result<String, String> {
    let src = config
        .sources
        .iter()
        .find(|s| s.label == source)
        .ok_or_else(|| format!("the configuration has no source labelled {source:?}"))?;
    let prefix = under.map(|u| format!("{}/", u.trim_end_matches('/')));
    let mut names: Vec<&str> = src
        .subvolumes
        .iter()
        .filter(|e| e.retired.is_none())
        .filter_map(|e| match &prefix {
            None => Some(e.name.as_str()),
            Some(p) => e
                .name
                .strip_prefix(p.as_str())
                .filter(|rest| !rest.is_empty() && !rest.contains('/')),
        })
        .collect();
    if names.is_empty() {
        return Err(format!(
            "source {source:?}{} selects no entry",
            under.map(|u| format!(" under {u:?}")).unwrap_or_default()
        ));
    }
    names.sort_by_key(|n| (n.to_lowercase(), n.to_string()));
    Ok(names.join(", "))
}

/// An opening marker's kind and attributes (`line` trimmed).
fn marker(line: &str) -> Result<(Kind, BTreeMap<String, String>), String> {
    let inner = line
        .strip_prefix(PREFIX)
        .and_then(|r| r.strip_suffix(END))
        .ok_or_else(|| format!("{line:?}: a marker opens and ends with --> on one line"))?;
    let mut words = inner.split_whitespace();
    let first = words.next().unwrap_or_default();
    let kind = Kind::from_word(first).ok_or_else(|| {
        format!("{line:?}: {first:?} is not a block (subvolumes, subvolume-table)")
    })?;
    let allowed = kind.attributes();
    let mut attrs = BTreeMap::new();
    for word in words {
        let (k, v) = word
            .split_once('=')
            .filter(|(_, v)| !v.is_empty())
            .ok_or_else(|| format!("{line:?}: {word:?} is not key=value"))?;
        if !allowed.contains(&k) {
            let names: Vec<String> = allowed.iter().map(|a| format!("{a}=")).collect();
            return Err(format!(
                "{line:?}: {k:?} is not an attribute ({})",
                names.join(", ")
            ));
        }
        if attrs.insert(k.to_string(), v.to_string()).is_some() {
            return Err(format!("{line:?}: {k:?} given twice"));
        }
    }
    let required = allowed[0];
    if !attrs.contains_key(required) {
        return Err(format!("{line:?}: no {required}= named"));
    }
    Ok((kind, attrs))
}

/// The current text for one block.
fn fill(
    config: &Config,
    kind: Kind,
    attrs: &BTreeMap<String, String>,
    mounts: &Mounts,
) -> Result<String, String> {
    let under = attrs.get("under").map(String::as_str);
    match kind {
        Kind::List => render(config, &attrs["source"], under),
        Kind::Table => {
            let top = match attrs.get("level").map(String::as_str) {
                None => false,
                Some("top") => true,
                Some(other) => return Err(format!("level={other:?}: only level=top")),
            };
            render_table(config, &attrs["volume"], under, top, mounts.get()?)
        }
    }
}

/// `text` with every marked block filled from `config` (and, for a table,
/// `mounts`); `Ok(None)` when nothing would change.
pub fn refreshed(config: &Config, text: &str, mounts: &Mounts) -> Result<Option<String>, String> {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<char> = None;
    // The open line's number, its kind and the block's new text, while inside one.
    let mut block: Option<(usize, Kind, String)> = None;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let n = i + 1;
        let t = line.trim();
        if let Some((at, kind, new)) = &block {
            if t.starts_with(PREFIX) {
                return Err(format!(
                    "line {n}: a marker opens inside another (line {at})"
                ));
            }
            if t.starts_with(CLOSE_PREFIX) {
                if t != kind.close() {
                    return Err(format!(
                        "line {n}: {t:?} closes the block opened on line {at}, which {} closes",
                        kind.close()
                    ));
                }
                out.push_str(new);
                out.push_str(line);
                block = None;
            }
            continue;
        }
        if let Some(f) = fence {
            if t.starts_with(&f.to_string().repeat(3)) {
                fence = None;
            }
            out.push_str(line);
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = t.chars().next();
        } else if t.starts_with(PREFIX) {
            let (kind, attrs) = marker(t).map_err(|e| format!("line {n}: {e}"))?;
            let body = fill(config, kind, &attrs, mounts).map_err(|e| format!("line {n}: {e}"))?;
            let eol = if line.ends_with("\r\n") { "\r\n" } else { "\n" };
            let body: String = body.lines().map(|l| format!("{l}{eol}")).collect();
            block = Some((n, kind, body));
        } else if t.starts_with(CLOSE_PREFIX) {
            return Err(format!("line {n}: a closing marker with no opening one"));
        }
        out.push_str(line);
    }
    if let Some((at, kind, _)) = block {
        return Err(format!(
            "line {at}: a marker with no closing {}",
            kind.close()
        ));
    }
    Ok((out != text).then_some(out))
}

/// The table for `volume`: every non-retired entry of every source on it,
/// one row each, sorted by name; then the patterns never backed up.
///
/// The mount point comes from `mounts`, matched by the source's filesystem
/// UUID (`device = "UUID=…"`) and the entry's path from the top of the
/// filesystem. An entry not mounted itself sits inside its nearest mounted
/// ancestor, the filesystem's top level last of all. Compression is one line
/// under the table, not a column: btrfs applies `compress=` to the whole
/// filesystem (the first mount's value wins), so a per-subvolume value would
/// say what fstab asks, not what runs. The rest comes from the renderer's own
/// helpers, so the table says what `btrbk.conf` does.
fn render_table(
    config: &Config,
    volume: &str,
    under: Option<&str>,
    top: bool,
    mounts: &[Mount],
) -> Result<String, String> {
    let want = crate::btrbk_conf::normalise_dir(volume);
    let sources: Vec<_> = config
        .sources
        .iter()
        .filter(|s| crate::btrbk_conf::normalise_dir(&s.volume) == want)
        .collect();
    if sources.is_empty() {
        return Err(format!(
            "the configuration has no source on volume {volume:?}"
        ));
    }
    let prefix = under.map(|u| format!("{}/", u.trim_end_matches('/')));
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut compression: Vec<String> = Vec::new();
    for src in sources {
        let uuid = src.device.strip_prefix("UUID=").ok_or_else(|| {
            format!(
                "source {:?}: device {:?} is not UUID=…, so its mounts cannot be told apart",
                src.label, src.device
            )
        })?;
        let on_fs: Vec<&Mount> = mounts.iter().filter(|m| m.uuid == uuid).collect();
        compression.extend(on_fs.iter().map(|m| compression_of(&m.options)));
        let targets = source_targets(config, src);
        let targets = if src.target_labels.is_empty() {
            "all".to_string()
        } else {
            let labels: Vec<&str> = targets.iter().map(|t| t.label.as_str()).collect();
            labels.join(", ")
        };
        let names = resolve_snapshot_names(&src.subvolumes);
        for (e, snap) in src.subvolumes.iter().zip(names) {
            if e.retired.is_some()
                || top && e.name.contains('/')
                || prefix
                    .as_deref()
                    .is_some_and(|p| !e.name.starts_with(p) || e.name.len() == p.len())
            {
                continue;
            }
            let mount =
                placed(&e.name, &on_fs, &src.volume).unwrap_or_else(|| "not mounted".to_string());
            let mut backed = format!("`{}/{snap}.<TS>`", source_subdir(src));
            if e.after_index {
                backed.push_str(", after indexing");
            }
            let row = format!(
                "| `{}` | {} | {} | {backed} | {} |",
                cell(&e.name),
                cell(&mount),
                cell(&src.label),
                cell(&targets)
            );
            rows.push((e.name.clone(), row));
        }
    }
    if rows.is_empty() {
        return Err(format!(
            "volume {volume:?}{} selects no entry",
            under.map(|u| format!(" under {u:?}")).unwrap_or_default()
        ));
    }
    rows.sort_by(|a, b| (a.0.to_lowercase(), &a.0).cmp(&(b.0.to_lowercase(), &b.0)));
    let mut out = String::from(
        "| Subvolume | Mount point | Source | Backed up as | Targets |\n\
         | --- | --- | --- | --- | --- |\n",
    );
    for (_, row) in rows {
        out.push_str(&row);
        out.push('\n');
    }
    compression.sort();
    compression.dedup();
    let compression = if compression.is_empty() {
        "unknown (not mounted)".to_string()
    } else {
        compression.join(", ")
    };
    out.push_str(&format!(
        "\nCompression, for the whole filesystem (its live mount options): {}\n",
        cell(&compression)
    ));
    let mut never = config.exclude_patterns();
    never.sort();
    if !never.is_empty() {
        let quoted: Vec<String> = never.iter().map(|p| format!("`{}`", cell(p))).collect();
        out.push_str(&format!(
            "\nNever backed up (`[subvolumes].exclude` and the built-in exclusions): {}\n",
            quoted.join(", ")
        ));
    }
    Ok(out)
}

/// Where entry `name` is reached: its own mount, else its nearest mounted
/// ancestor's plus the rest of the path, the filesystem's top level last
/// (preferring a mount other than the backup's own `volume`). Several
/// mounts of one subvolume (a bind mount): the shortest path, then the first
/// in byte order, so the choice is the same every run. The top level (`.`)
/// is the volume itself.
fn placed(name: &str, on_fs: &[&Mount], volume: &str) -> Option<String> {
    if name == "." {
        return on_fs
            .iter()
            .any(|m| m.fsroot == "/")
            .then(|| volume.to_string());
    }
    let path = format!("/{name}");
    let inside = |m: &&&Mount| {
        m.fsroot == path || m.fsroot == "/" || path.starts_with(&format!("{}/", m.fsroot))
    };
    let vol = crate::btrbk_conf::normalise_dir(volume);
    let m = on_fs.iter().filter(inside).min_by(|a, b| {
        let rank = |m: &Mount| {
            (
                std::cmp::Reverse(if m.fsroot == "/" { 0 } else { m.fsroot.len() }),
                crate::btrbk_conf::normalise_dir(&m.target) == vol,
                m.target.len(),
                m.target.clone(),
            )
        };
        rank(a).cmp(&rank(b))
    })?;
    if m.fsroot == path {
        return Some(m.target.clone());
    }
    let rest = if m.fsroot == "/" {
        &path[1..]
    } else {
        &path[m.fsroot.len() + 1..]
    };
    Some(if m.target == "/" {
        format!("/{rest}")
    } else {
        format!("{}/{rest}", m.target)
    })
}

/// A mount's compression, from its options: the `compress=` or
/// `compress-force=` value, `nodatacow`, or `none`.
fn compression_of(options: &str) -> String {
    for o in options.split(',') {
        if o == "nodatacow" {
            return "nodatacow".into();
        }
        if let Some(v) = o
            .strip_prefix("compress-force=")
            .or_else(|| o.strip_prefix("compress="))
        {
            return v.to_string();
        }
    }
    "none".into()
}

/// Text safe inside a table cell.
fn cell(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Refresh every regular `*.md` directly in `dir`; with `write` false, only
/// say what would change. A directory that does not exist has no documents.
pub fn refresh_dir(
    config: &Config,
    dir: &Path,
    write: bool,
    mounts: &Mounts,
) -> Result<Vec<DocReport>, String> {
    let listing = match fs::read_dir(dir) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot list {}: {e}", dir.display())),
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in listing {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.extension().is_some_and(|x| x == "md") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|path| {
            let outcome = refresh_one(config, &path, write, mounts);
            DocReport { path, outcome }
        })
        .collect())
}

/// The exit status of a refresh: 2 when any document was refused, else 1
/// when checking found one that would change, else 0.
pub fn exit_code(reports: &[DocReport], write: bool) -> i32 {
    if reports
        .iter()
        .any(|r| matches!(r.outcome, DocOutcome::Refused(_)))
    {
        2
    } else if !write && reports.iter().any(|r| r.outcome == DocOutcome::Updated) {
        1
    } else {
        0
    }
}

fn refresh_one(config: &Config, path: &Path, write: bool, mounts: &Mounts) -> DocOutcome {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            return DocOutcome::Refused("a symlink, never followed".into());
        }
        Ok(m) if !m.is_file() => return DocOutcome::Refused("not a regular file".into()),
        Ok(_) => {}
        Err(e) => return DocOutcome::Refused(format!("cannot read it: {e}")),
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return DocOutcome::Refused(format!("cannot read it: {e}")),
    };
    match refreshed(config, &text, mounts) {
        Err(why) => DocOutcome::Refused(why),
        Ok(None) => DocOutcome::Current,
        Ok(Some(new)) => match write.then(|| crate::fsutil::write_atomic(path, &new)) {
            Some(Err(e)) => DocOutcome::Refused(format!("cannot write it: {e}")),
            _ => DocOutcome::Updated,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Retention, Source, SubvolConfig, Target, TargetRole};

    /// A probe a list block must never call.
    fn no_mounts() -> Mounts<'static> {
        Mounts::new(|| panic!("only a table reads the mounts"))
    }

    fn entry(name: &str, retired: Option<&str>) -> SubvolConfig {
        let mut e: SubvolConfig = toml::from_str(&format!("name = {name:?}")).unwrap();
        e.retired = retired.map(str::to_string);
        e
    }

    fn config() -> Config {
        let mut c = Config::default();
        let mut s: Source = toml::from_str(
            "label = \"projects\"\nvolume = \"/srv/vol\"\ndevice = \"/dev/null\"\nsubvolumes = []\n",
        )
        .unwrap();
        s.subvolumes = vec![
            entry("Work", None),
            entry("Work/beta", None),
            entry("Work/Alpha", None),
            entry("Work/gone", Some("2026-01-01")),
            entry("Work/beta/nested", None),
            entry("Other", None),
        ];
        c.sources.push(s);
        c
    }

    const OPEN_LINE: &str = "<!-- btrdasd:subvolumes source=projects under=Work -->";

    fn doc(body: &str) -> String {
        format!("# Title\n\nIntro, kept.\n\n{OPEN_LINE}\n{body}\n{CLOSE}\n\nAfter, kept.\n")
    }

    #[test]
    fn a_block_lists_the_direct_children_not_retired_sorted() {
        let out = refreshed(&config(), &doc("stale, list"), &no_mounts())
            .unwrap()
            .unwrap();
        assert_eq!(out, doc("Alpha, beta"));
    }

    #[test]
    fn without_under_every_entry_by_its_full_name() {
        let text = "<!-- btrdasd:subvolumes source=projects -->\nx\n<!-- /btrdasd:subvolumes -->\n";
        let out = refreshed(&config(), text, &no_mounts()).unwrap().unwrap();
        assert_eq!(
            out,
            "<!-- btrdasd:subvolumes source=projects -->\nOther, Work, Work/Alpha, Work/beta, Work/beta/nested\n<!-- /btrdasd:subvolumes -->\n"
        );
    }

    #[test]
    fn a_current_block_and_a_file_without_markers_are_left_alone() {
        assert_eq!(
            refreshed(&config(), &doc("Alpha, beta"), &no_mounts()).unwrap(),
            None
        );
        assert_eq!(
            refreshed(&config(), "# Nothing marked\n", &no_mounts()).unwrap(),
            None
        );
    }

    #[test]
    fn unpaired_or_unknown_markers_are_refused() {
        let cases = [
            (format!("{OPEN_LINE}\nlist\n"), "no closing"),
            (
                format!("{OPEN_LINE}\n{OPEN_LINE}\n{CLOSE}\n"),
                "inside another",
            ),
            (format!("text\n{CLOSE}\n"), "no opening"),
            (
                format!("<!-- btrdasd:subvolumes source=nope -->\n{CLOSE}\n"),
                "no source",
            ),
            (
                format!("<!-- btrdasd:subvolumes source=projects colour=red -->\n{CLOSE}\n"),
                "colour",
            ),
            (
                format!("<!-- btrdasd:subvolumes under=Work -->\n{CLOSE}\n"),
                "source=",
            ),
            (
                format!("<!-- btrdasd:subvolumes source=projects under=Nowhere -->\n{CLOSE}\n"),
                "no entry",
            ),
        ];
        for (text, want) in cases {
            let err = refreshed(&config(), &text, &no_mounts()).expect_err(&text);
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }

    #[test]
    fn markers_inside_fenced_code_are_text() {
        let text = format!("```text\n{OPEN_LINE}\nexample\n```\n~~~\n{CLOSE}\n~~~\n");
        assert_eq!(refreshed(&config(), &text, &no_mounts()).unwrap(), None);
    }

    #[test]
    fn a_directory_refreshes_its_documents_and_refuses_what_it_cannot_vouch_for() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::write(p.join("stale.md"), doc("old")).unwrap();
        fs::write(p.join("current.md"), doc("Alpha, beta")).unwrap();
        fs::write(p.join("broken.md"), format!("{OPEN_LINE}\nhand-written\n")).unwrap();
        fs::write(p.join("notes.txt"), doc("old")).unwrap();
        std::os::unix::fs::symlink(p.join("stale.md"), p.join("link.md")).unwrap();

        let checked = refresh_dir(&config(), p, false, &no_mounts()).unwrap();
        assert_eq!(
            fs::read_to_string(p.join("stale.md")).unwrap(),
            doc("old"),
            "a check writes nothing"
        );
        let outcome = |r: &[DocReport], n: &str| {
            r.iter()
                .find(|d| d.path == p.join(n))
                .map(|d| d.outcome.clone())
        };
        assert_eq!(outcome(&checked, "stale.md"), Some(DocOutcome::Updated));

        let mtime = |n: &str| fs::metadata(p.join(n)).unwrap().modified().unwrap();
        let current_before = mtime("current.md");
        let done = refresh_dir(&config(), p, true, &no_mounts()).unwrap();
        assert_eq!(outcome(&done, "stale.md"), Some(DocOutcome::Updated));
        assert_eq!(outcome(&done, "current.md"), Some(DocOutcome::Current));
        assert!(matches!(
            outcome(&done, "broken.md"),
            Some(DocOutcome::Refused(_))
        ));
        assert!(
            matches!(outcome(&done, "link.md"), Some(DocOutcome::Refused(w)) if w.contains("symlink"))
        );
        assert_eq!(outcome(&done, "notes.txt"), None, "only *.md");
        assert_eq!(
            fs::read_to_string(p.join("stale.md")).unwrap(),
            doc("Alpha, beta")
        );
        assert_eq!(
            fs::read_to_string(p.join("broken.md")).unwrap(),
            format!("{OPEN_LINE}\nhand-written\n"),
            "a refused file is left byte for byte"
        );
        assert_eq!(
            mtime("current.md"),
            current_before,
            "a current file is not rewritten"
        );
        assert!(
            refresh_dir(&config(), &p.join("absent"), true, &no_mounts())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            exit_code(&done, true),
            2,
            "a refused document fails the refresh"
        );
    }

    #[test]
    fn a_check_that_would_change_something_exits_1_and_a_write_0() {
        let r = |o| {
            vec![DocReport {
                path: "a.md".into(),
                outcome: o,
            }]
        };
        assert_eq!(exit_code(&r(DocOutcome::Updated), false), 1);
        assert_eq!(exit_code(&r(DocOutcome::Updated), true), 0);
        assert_eq!(exit_code(&r(DocOutcome::Current), false), 0);
        assert_eq!(exit_code(&r(DocOutcome::Refused("x".into())), false), 2);
        assert_eq!(exit_code(&[], false), 0);
    }

    const UUID: &str = "5eed0001-0000-4000-8000-000000000001";

    fn sub(name: &str) -> SubvolConfig {
        SubvolConfig {
            name: name.into(),
            ..Default::default()
        }
    }

    fn source(label: &str, volume: &str, subdirs: &[&str], labels: &[&str]) -> Source {
        Source {
            label: label.into(),
            volume: volume.into(),
            subvolumes: Vec::new(),
            device: format!("UUID={UUID}"),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: subdirs.iter().map(|s| s.to_string()).collect(),
            target_labels: labels.iter().map(|s| s.to_string()).collect(),
            static_subvolumes: Vec::new(),
        }
    }

    fn target(label: &str, role: TargetRole) -> Target {
        Target {
            label: label.into(),
            serial: "SERIAL01".into(),
            serials: Vec::new(),
            mount_uuid: None,
            mount: format!("/mnt/{label}"),
            role,
            retention: Retention {
                daily: 7,
                weekly: 0,
                monthly: 0,
                yearly: 0,
            },
            display_name: String::new(),
            esp_partuuid: None,
        }
    }

    /// Two sources on one volume, one on another; nested entries, an
    /// explicit snapshot name, the state pass, a retired entry.
    fn table_config() -> Config {
        let mut c = Config::default();
        c.targets.push(target("big", TargetRole::Primary));
        c.targets.push(target("small", TargetRole::Mirror));
        let mut base = source("base", "/srv/vol", &["base"], &[]);
        let mut home = sub("@home");
        home.snapshot_name = Some("home".into());
        let mut state = sub("@/var/lib/state");
        state.snapshot_name = Some("state".into());
        state.after_index = true;
        let mut gone = sub("@gone");
        gone.retired = Some("2026-01-01".into());
        base.subvolumes = vec![
            sub("@data/inner"),
            home,
            sub("@"),
            state,
            gone,
            sub("@data"),
            sub("@loose"),
        ];
        let mut bulk = source("bulk", "/srv/vol/", &[], &["big"]);
        bulk.subvolumes = vec![sub("@vm")];
        let mut other = source("other", "/srv/elsewhere", &[], &[]);
        other.subvolumes = vec![sub("@x")];
        c.sources = vec![base, bulk, other];
        c.subvolumes.exclude = vec!["@cache".into()];
        c
    }

    /// `findmnt -P` as this fixture's host would print it.
    fn findmnt() -> String {
        let other = "5eed0002-0000-4000-8000-000000000002";
        [
            format!(r#"TARGET="/srv/vol" UUID="{UUID}" FSROOT="/" OPTIONS="rw,compress=zstd:3""#),
            format!(r#"TARGET="/top" UUID="{UUID}" FSROOT="/" OPTIONS="rw,compress=zstd:3""#),
            format!(r#"TARGET="/mnt/bind-home" UUID="{UUID}" FSROOT="/@home" OPTIONS="rw,compress=zstd:1""#),
            format!(r#"TARGET="/" UUID="{UUID}" FSROOT="/@" OPTIONS="rw,noatime,compress=zstd:1,subvol=/@""#),
            format!(r#"TARGET="/home" UUID="{UUID}" FSROOT="/@home" OPTIONS="rw,compress=zstd:1""#),
            format!(r#"TARGET="/data\x20set" UUID="{UUID}" FSROOT="/@data" OPTIONS="rw,nodatacow""#),
            format!(r#"TARGET="/elsewhere/vm" UUID="{other}" FSROOT="/@vm" OPTIONS="rw""#),
        ]
        .join("\n")
            + "\n"
    }

    fn mounts() -> Mounts<'static> {
        Mounts::new(|| parse_mounts(&findmnt()))
    }

    const TABLE: &str = "\
| Subvolume | Mount point | Source | Backed up as | Targets |
| --- | --- | --- | --- | --- |
| `@` | / | base | `base/root.<TS>` | all |
| `@/var/lib/state` | /var/lib/state | base | `base/state.<TS>`, after indexing | all |
| `@data` | /data set | base | `base/data.<TS>` | all |
| `@data/inner` | /data set/inner | base | `base/data-inner.<TS>` | all |
| `@home` | /home | base | `base/home.<TS>` | all |
| `@loose` | /top/@loose | base | `base/loose.<TS>` | all |
| `@vm` | /top/@vm | bulk | `bulk/vm.<TS>` | big |

Compression, for the whole filesystem (its live mount options): nodatacow, zstd:1, zstd:3

Never backed up (`[subvolumes].exclude` and the built-in exclusions): `@cache`, `@tmp`, `@var-tmp`
";

    fn table_doc(open: &str, body: &str) -> String {
        format!("Intro.\n\n{open}\n{body}<!-- /btrdasd:subvolume-table -->\nAfter.\n")
    }

    #[test]
    fn a_table_lists_every_entry_on_the_volume_with_its_live_mount() {
        let open = "<!-- btrdasd:subvolume-table volume=/srv/vol -->";
        let out = refreshed(&table_config(), &table_doc(open, "old\n"), &mounts())
            .unwrap()
            .unwrap();
        assert_eq!(out, table_doc(open, TABLE));
        assert_eq!(
            refreshed(&table_config(), &out, &mounts()).unwrap(),
            None,
            "a second refresh changes nothing: the table is byte-stable"
        );
    }

    #[test]
    fn level_top_keeps_only_the_entries_at_the_top_of_the_volume() {
        let open = "<!-- btrdasd:subvolume-table volume=/srv/vol level=top -->";
        let out = refreshed(&table_config(), &table_doc(open, ""), &mounts())
            .unwrap()
            .unwrap();
        let rows: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("| `"))
            .map(|l| l.split('|').nth(1).unwrap().trim())
            .collect();
        assert_eq!(rows, ["`@`", "`@data`", "`@home`", "`@loose`", "`@vm`"]);
    }

    #[test]
    fn a_table_is_refused_when_the_mounts_cannot_be_read_or_nothing_matches() {
        let failing = Mounts::new(|| Err("findmnt exited 1: boom".to_string()));
        let open = "<!-- btrdasd:subvolume-table volume=/srv/vol -->";
        let err = refreshed(&table_config(), &table_doc(open, ""), &failing).unwrap_err();
        assert!(err.contains("boom"), "{err}");
        for (open, want) in [
            (
                "<!-- btrdasd:subvolume-table volume=/nowhere -->",
                "no source",
            ),
            ("<!-- btrdasd:subvolume-table source=base -->", "source"),
            (
                "<!-- btrdasd:subvolume-table volume=/srv/vol level=2 -->",
                "level=top",
            ),
        ] {
            let err = refreshed(&table_config(), &table_doc(open, ""), &mounts()).unwrap_err();
            assert!(err.contains(want), "{open}: {err}");
        }
        // Only another filesystem is mounted: nothing here is reached.
        let elsewhere = Mounts::new(|| {
            parse_mounts(
                r#"TARGET="/elsewhere/vm" UUID="5eed0002-0000-4000-8000-000000000002" FSROOT="/@vm" OPTIONS="rw""#,
            )
        });
        let open = "<!-- btrdasd:subvolume-table volume=/srv/vol -->";
        let out = refreshed(&table_config(), &table_doc(open, ""), &elsewhere)
            .unwrap()
            .unwrap();
        assert!(out.contains("| `@vm` | not mounted | bulk |"), "{out}");
        assert!(
            out.contains("live mount options): unknown (not mounted)"),
            "{out}"
        );
        let crossed =
            "<!-- btrdasd:subvolume-table volume=/srv/vol -->\n<!-- /btrdasd:subvolumes -->\n";
        let err = refreshed(&table_config(), crossed, &mounts()).unwrap_err();
        assert!(err.contains("closes the block"), "{err}");
    }

    #[test]
    fn findmnt_pairs_parse_and_a_short_line_is_an_error() {
        let m = parse_mounts(&findmnt()).unwrap();
        assert_eq!(m.len(), 7);
        assert_eq!(m[5].target, "/data set");
        assert_eq!(m[3].fsroot, "/@");
        assert!(
            parse_mounts(r#"TARGET="/" UUID="u" FSROOT="/@""#)
                .unwrap_err()
                .contains("OPTIONS")
        );
        assert!(parse_mounts("garbage").is_err());
    }
}
