//! What the image itself says boot puts under `/run` (bd DAS-Backup-Manager-3nb).
//!
//! `/run` is a tmpfs: empty at rest, so what is read from the disk's copy of
//! it is never what boot sees there, and a path under it is unknown. But the
//! image declares part of what its boot creates there — `tmpfiles.d(5)`:
//! `d /run/initramfs`, `f+ /run/initramfs/mkinitcpio-shutdown.conf … HOOKS=(…)`
//! — and a path it declares is read as the file or directory that line
//! makes, as if the image held it.
//!
//! This rests on the limit the module already declares for what a running
//! system puts in place: a path declared here is still unknown to anything
//! that writes it besides `systemd-tmpfiles` (a service, the initramfs, a
//! `/run/tmpfiles.d` the booted system fills); this reads the image's
//! declaration alone. Everything below is the other direction: a path is
//! declared only when nothing in the image could make it anything else.
//!
//! A path is declared when all of these hold:
//!
//! - every `*.conf` of `etc/tmpfiles.d`, `usr/local/lib/tmpfiles.d` and
//!   `usr/lib/tmpfiles.d` was read (a name in a higher directory replaces the
//!   same name in a lower one; one masked by a link to `/dev/null`, or empty,
//!   is gone); one that cannot be read, a directory that is no directory, or
//!   a line about `/run` that cannot be parsed, declares nothing at all;
//! - exactly one line names it, of type `f`, `f+` (a file, its argument the
//!   content) or `d` (a directory), with at most the modifiers `+`, `-` and
//!   `!`;
//! - no line of another type names a parent of it, and no line with a glob
//!   matches it or a parent;
//! - no `%` specifier is in its line (`%t` is `/run`), no `..` in a path, no
//!   backslash in a field before the argument (systemd decodes C escapes in
//!   every field, so a path may be `/\x72un/…`), and the content has only
//!   escapes this reads (`\n`, `\t`, `\r`, `\\`, `\xNN`, octal) and no quote;
//! - no line could reach `/run` through a link ([`outside_is_plain`]): no
//!   `L` line under it, and no line of any type elsewhere with a parent that
//!   is a link the image makes (`L`, a glob counts) or one on its disk (each
//!   name a glob may stand for is looked at), or under `/proc`
//!   (`/proc/self/root/run`) but `/proc/sys`; a line that writes (`f`, `F`,
//!   `w`, `C`) also when its own name is such a link or it is under `/dev`.
//!   `/sys` and `/proc/sys` are exempt; any other declares nothing.
//!
//! `/var/run` and `/var/lock` are read as `/run` and `/run/lock` (Arch's
//! `filesystem` links them there).

use std::collections::BTreeMap;
use std::path::Path;

use super::{
    Entry as OnDisk, FileRead, Listing, MountFlags, Mounts, entry_at, list_dir, read_file,
};

/// Where tmpfiles.d is read, highest precedence first (`tmpfiles.d(5)`,
/// less `/run/tmpfiles.d`, which is empty at rest).
const DIRS: [&str; 3] = [
    "etc/tmpfiles.d",
    "usr/local/lib/tmpfiles.d",
    "usr/lib/tmpfiles.d",
];

/// What a declared path is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Entry {
    /// A directory (`d`); what is in it is not declared.
    Dir,
    /// A regular file (`f`, `f+`) with this content.
    File(String),
}

/// One line of a tmpfiles.d file about a path under `/run`.
#[derive(Debug)]
struct Line {
    /// The type letter, `+` etc. taken off.
    kind: char,
    /// Its other modifiers (`+`, `-`, `!`, `=`, `~`, `^`).
    modifiers: String,
    /// Absolute, simplified, `/var/run` read as `/run`.
    path: String,
    /// The last field, as written.
    argument: Option<String>,
}

/// A line outside `/run`: its path may lead into it through a link
/// ([`outside_is_plain`]).
#[derive(Debug)]
struct Outside {
    /// Its type letter.
    kind: char,
    /// Absolute, simplified; a glob's characters as written.
    path: String,
}

/// The lines of an image's tmpfiles.d that touch `/run`.
#[derive(Debug)]
pub(super) struct Declared {
    lines: Vec<Line>,
}

/// What reading the image's tmpfiles.d gave: lines, or why none can be
/// trusted.
#[derive(Debug)]
pub(super) enum Declarations {
    Read(Declared),
    /// A file or directory that could not be read, or a line that could not
    /// be parsed: nothing is declared.
    Unread,
}

impl Declarations {
    /// What the image declares at `rel` (root-relative, `run/…`), if it can be
    /// told.
    pub(super) fn entry(&self, rel: &str) -> Option<Entry> {
        match self {
            Declarations::Read(declared) => declared.entry(rel),
            Declarations::Unread => None,
        }
    }
}

/// The image's tmpfiles.d lines about `/run`: each `*.conf`, a higher
/// directory's replacing the same name in a lower one.
pub(super) fn read_declarations(root: &Path, probe: MountFlags, mounts: &Mounts) -> Declarations {
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for dir in DIRS {
        match list_dir(root, dir) {
            Listing::Absent => {}
            Listing::Unreadable(_) => return Declarations::Unread,
            Listing::Names(names) => {
                for name in names.into_iter().filter(|n| n.ends_with(".conf")) {
                    files.entry(name.clone()).or_insert(format!("{dir}/{name}"));
                }
            }
        }
    }
    let mut lines = Vec::new();
    let mut outside = Vec::new();
    for rel in files.values() {
        match read_file(root, rel, probe, Some(mounts)) {
            FileRead::Text(text) => match parse(&text) {
                Ok((found, more)) => {
                    lines.extend(found);
                    outside.extend(more);
                }
                Err(_) => return Declarations::Unread,
            },
            // Masked, empty, or no regular file: systemd reads none of it.
            FileRead::Nothing | FileRead::NotFile => {}
            FileRead::Unread(_) | FileRead::Unknown(_) => return Declarations::Unread,
        }
    }
    if !outside_is_plain(root, &outside) {
        return Declarations::Unread;
    }
    Declarations::Read(Declared { lines })
}

/// Whether no line outside `/run` can reach into it through a link: a file
/// made (`f`, `f+`, `F`), written (`w`), copied (`C`) or anything else made
/// under a parent that is a link lands where the link leads. A line is plain
/// when its path is under `/sys` or `/proc/sys` (never links into `/run`), or
/// all of these hold:
///
/// - it is not under `/proc` (`/proc/self/root/run`), nor, for a line that
///   writes (`f`, `F`, `w`, `C`), under `/dev` (`/dev/fd`);
/// - no link the image makes (`L`, a glob counts) is a parent of it, or for
///   a line that writes, it;
/// - on the image's disk no parent is a link (each name a glob may stand for
///   is looked at), nor, for a line that writes, it, unless it ends in a
///   glob: the names that glob matches are not followed.
fn outside_is_plain(root: &Path, outside: &[Outside]) -> bool {
    let links: Vec<&str> = outside
        .iter()
        .filter(|o| o.kind == 'L')
        .map(|o| o.path.as_str())
        .collect();
    for line in outside {
        let path = line.path.as_str();
        if ["/sys/", "/proc/sys/"]
            .iter()
            .any(|dir| path.starts_with(dir))
        {
            continue;
        }
        let writes = matches!(line.kind, 'f' | 'F' | 'w' | 'C');
        if under(path, "/proc") || (writes && under(path, "/dev")) {
            return false;
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let mut at = if writes { path } else { parent };
        while !at.is_empty() {
            // Either may be the glob.
            if links
                .iter()
                .any(|link| may_match(link, at) || may_match(at, link))
            {
                return false;
            }
            at = at.rsplit_once('/').map_or("", |(up, _)| up);
        }
        if parent_is_link(root, parent) {
            return false;
        }
        if writes && !has_glob(name) && is_link(root, path) {
            return false;
        }
    }
    true
}

/// Whether the entry at `path` (no glob) is a link, or cannot be told.
fn is_link(root: &Path, path: &str) -> bool {
    matches!(
        entry_at(root, path.trim_start_matches('/')),
        OnDisk::Symlink(_) | OnDisk::Unreadable(_)
    )
}

/// How many names a glob in a parent may stand for before this gives up.
const MAX_NAMES: usize = 4096;

/// Whether `parent`, a directory path (a glob in any name), or a directory on
/// its way is a link on the image's disk, or cannot be told: each name a glob
/// matches on disk is looked at.
fn parent_is_link(root: &Path, parent: &str) -> bool {
    let mut layer = vec![String::new()];
    for part in parent.split('/').filter(|p| !p.is_empty()) {
        let mut next = Vec::new();
        for base in &layer {
            if has_glob(part) {
                match list_dir(root, base) {
                    Listing::Absent => {}
                    Listing::Unreadable(_) => return true,
                    Listing::Names(names) => next.extend(
                        names
                            .into_iter()
                            .filter(|name| may_match(part, name))
                            .map(|name| format!("{base}/{name}").trim_start_matches('/').into()),
                    ),
                }
            } else {
                next.push(format!("{base}/{part}").trim_start_matches('/').to_string());
            }
        }
        if next.len() > MAX_NAMES || next.iter().any(|rel| is_link(root, rel)) {
            return true;
        }
        layer = next;
    }
    false
}

/// Whether `path` is `dir` or under it.
fn under(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `/run` as one path: `/var/run` and `/var/lock` lead there. `None` when the
/// path is not under it. A glob's text before its first glob character
/// counts as a path that may be.
fn under_run(path: &str) -> Option<String> {
    let mapped = if let Some(rest) = path.strip_prefix("/var/run") {
        format!("/run{rest}")
    } else if let Some(rest) = path.strip_prefix("/var/lock") {
        format!("/run/lock{rest}")
    } else {
        path.to_string()
    };
    let inside = mapped == "/run" || mapped.starts_with("/run/");
    // A glob can start short of /run (`/r*`, `/*/x`): its prefix before the
    // first glob character is then a start of "/run/…".
    let prefix = mapped.split(['*', '?', '[']).next().unwrap_or_default();
    let may = has_glob(&mapped)
        && ["/run/", "/var/run/", "/var/lock/"]
            .iter()
            .any(|dir| dir.starts_with(prefix));
    (inside || may).then_some(mapped)
}

/// Whether `path` has a glob character.
fn has_glob(path: &str) -> bool {
    path.contains(['*', '?', '[', '\\'])
}

/// The lines of one file that touch `/run`, and those elsewhere; an error when a line cannot be told (a backslash in a
/// field: systemd decodes C escapes in every field, so it may be any path,
/// `/run` included).
fn parse(text: &str) -> Result<(Vec<Line>, Vec<Outside>), String> {
    let mut found = Vec::new();
    let mut outside = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let fields = split_fields(line);
        // `%t` and its kin may stand for /run: a line that starts a path with
        // one is any path.
        if let Some(Some(path)) = fields.as_ref().map(|f| f.get(1))
            && path.starts_with('%')
        {
            return Err(format!("{line}: a specifier at the start of a path"));
        }
        let Some(fields) = fields else {
            return Err(format!("{line}: not readable as a line"));
        };
        // A line with no path names nothing.
        let (Some(kind), Some(path)) = (fields.first(), fields.get(1)) else {
            continue;
        };
        if path.split('/').any(|part| part == "..") {
            // `..` can lead into /run from anywhere.
            return Err(format!("{line}: a path with `..` in it"));
        }
        let path = widen_specifiers(path);
        let Some(simple) = simplify(&path) else {
            continue;
        };
        let mut letters = kind.chars();
        let Some(base) = letters.next() else {
            continue;
        };
        let Some(path) = under_run(&simple) else {
            outside.push(Outside {
                kind: base,
                path: simple,
            });
            continue;
        };
        if base == 'L' {
            // A link under /run is written through by `w` and `C`.
            return Err(format!("{line}: a link under /run"));
        }
        found.push(Line {
            kind: base,
            modifiers: letters.collect(),
            path,
            // `-` leaves a field unset.
            argument: fields.get(6).filter(|a| a.as_str() != "-").cloned(),
        });
    }
    Ok((found, outside))
}

/// `path` with each specifier (`%b`, `%H`...) made a glob's `*`: whatever it
/// stands for, it is some text, and the path may be any that has some there.
fn widen_specifiers(path: &str) -> String {
    let mut out = String::new();
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            chars.next();
            out.push('*');
        } else {
            out.push(c);
        }
    }
    out
}

/// `path` without a trailing slash, doubled slashes or `.` components; `None`
/// when it is not absolute.
fn simplify(path: &str) -> Option<String> {
    if !path.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    Some(format!("/{}", parts.join("/")))
}

/// The first six whitespace-separated fields of a line, a quote grouping a
/// field, then the rest of the line as the seventh (`tmpfiles.d(5)`: the
/// argument is the rest of the line). `None` when a quote or escape in a
/// field is one this does not read.
fn split_fields(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut rest = line.trim_start();
    while fields.len() < 6 && !rest.is_empty() {
        let mut field = String::new();
        let mut quote: Option<char> = None;
        let mut end = rest.len();
        for (i, c) in rest.char_indices() {
            match (quote, c) {
                (_, '\\') => return None,
                (None, '"' | '\'') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (None, c) if c.is_whitespace() => {
                    end = i;
                    break;
                }
                _ => field.push(c),
            }
        }
        if quote.is_some() {
            return None;
        }
        fields.push(field);
        rest = rest[end..].trim_start();
    }
    if !rest.is_empty() {
        fields.push(rest.trim_end().to_string());
    }
    Some(fields)
}

/// Whether `pattern` may match `text`: `*` any run of characters, `?` and a
/// `[…]` any one — wider than `fnmatch`, never narrower.
fn may_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some(('*', rest)) => (0..=t.len()).any(|n| go(rest, &t[n..])),
            Some(('?', rest)) => !t.is_empty() && go(rest, &t[1..]),
            Some(('[', rest)) => {
                let after = rest.iter().position(|c| *c == ']').map_or(0, |i| i + 1);
                !t.is_empty() && go(&rest[after..], &t[1..])
            }
            Some(('\\', rest)) => match rest.split_first() {
                Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
                None => false,
            },
            Some((c, rest)) => t.first() == Some(c) && go(rest, &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
}

/// A tmpfiles.d argument as `systemd-tmpfiles` writes it: `\n`, `\t`, `\r`,
/// `\\`, `\xNN` and octal escapes. `None` for another escape, a quote, a `%`
/// specifier or anything that is no UTF-8.
fn unescape(raw: &str) -> Option<String> {
    if raw.contains(['"', '\'', '%']) {
        return None;
    }
    let mut out: Vec<u8> = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let next = *bytes.get(i + 1)?;
        match next {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'\\' => out.push(b'\\'),
            b'x' => {
                let hex = raw.get(i + 2..i + 4)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            b'0'..=b'7' => {
                let octal = raw.get(i + 1..i + 4)?;
                out.push(u8::from_str_radix(octal, 8).ok()?);
                i += 2;
            }
            _ => return None,
        }
        i += 2;
    }
    String::from_utf8(out).ok()
}

impl Declared {
    /// What is declared at `rel`, root-relative (`run/initramfs`), if
    /// exactly one plain line declares it and nothing else could change it.
    fn entry(&self, rel: &str) -> Option<Entry> {
        let path = format!("/{}", rel.trim_matches('/'));
        if path != "/run" && !path.starts_with("/run/") {
            return None;
        }
        let mut own = None;
        for line in &self.lines {
            if has_glob(&line.path) {
                // A glob that may match the path, or a parent of it.
                let mut at = path.as_str();
                loop {
                    // The path, and its name through the link that leads to /run.
                    let alias = match at.strip_prefix("/run/lock") {
                        Some(rest) => format!("/var/lock{rest}"),
                        None => format!("/var{at}"),
                    };
                    if may_match(&line.path, at) || may_match(&line.path, &alias) {
                        return None;
                    }
                    match at.rfind('/') {
                        Some(0) | None => break,
                        Some(i) => at = &at[..i],
                    }
                }
            } else if line.path == path {
                if own.is_some() {
                    return None;
                }
                own = Some(line);
            } else if path.starts_with(&format!("{}/", line.path)) && line.kind != 'd' {
                // Something but a plain directory is made of a parent.
                return None;
            }
        }
        let own = own?;
        if !own.modifiers.chars().all(|c| matches!(c, '+' | '-' | '!')) {
            return None;
        }
        match own.kind {
            'd' => Some(Entry::Dir),
            'f' => unescape(own.argument.as_deref().unwrap_or_default()).map(Entry::File),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fields_of_a_line_and_its_argument_as_the_rest() {
        assert_eq!(
            split_fields("f+ /run/a 0644 root root - HOOKS=(a b)\\n").unwrap(),
            [
                "f+",
                "/run/a",
                "0644",
                "root",
                "root",
                "-",
                "HOOKS=(a b)\\n"
            ]
        );
        assert_eq!(
            split_fields("d \"/run/with space\" 0755").unwrap(),
            ["d", "/run/with space", "0755"]
        );
        assert!(split_fields("d /run/a\\040b").is_none());
        assert!(split_fields("d \"/run/a").is_none());
    }

    #[test]
    fn a_path_is_under_run_by_itself_through_a_link_or_as_a_glob_that_may_reach_it() {
        assert_eq!(under_run("/run/x").as_deref(), Some("/run/x"));
        assert_eq!(under_run("/var/run/x").as_deref(), Some("/run/x"));
        assert_eq!(under_run("/var/lock").as_deref(), Some("/run/lock"));
        assert_eq!(under_run("/r*/x").as_deref(), Some("/r*/x"));
        assert_eq!(under_run("/*").as_deref(), Some("/*"));
        assert_eq!(under_run("/runner"), None);
        assert_eq!(under_run("/var/lib/x"), None);
        // /var/* may be /var/run.
        assert_eq!(under_run("/var/*").as_deref(), Some("/var/*"));
        assert_eq!(under_run("/var/lib/*"), None);
    }

    #[test]
    fn a_glob_matches_wider_never_narrower() {
        assert!(may_match("/run/init*", "/run/initramfs"));
        assert!(may_match("/run/*", "/run/a/b"));
        assert!(may_match("/run/mk[i]n", "/run/mkin"));
        assert!(may_match("/run/?", "/run/a"));
        assert!(!may_match("/run/init*", "/run/other"));
        assert!(!may_match("/run/?", "/run/ab"));
    }

    #[test]
    fn an_argument_is_unescaped_as_tmpfiles_does_or_not_at_all() {
        assert_eq!(
            unescape("a\\nb\\\\c\\x41\\101").as_deref(),
            Some("a\nb\\cAA")
        );
        assert_eq!(unescape(""), Some(String::new()));
        assert!(unescape("\\q").is_none());
        assert!(unescape("\\x4").is_none());
        assert!(unescape("%t").is_none());
        assert!(unescape("'x'").is_none());
    }

    #[test]
    fn only_the_modifiers_plus_minus_and_bang_leave_a_line_declared() {
        let declared = |kind: &str| {
            let (lines, _) = parse(&format!("{kind} /run/x 0644 root root - A\n")).unwrap();
            Declared { lines }.entry("run/x")
        };
        assert_eq!(declared("f+!"), Some(Entry::File("A".into())));
        assert_eq!(declared("d-"), Some(Entry::Dir));
        for kind in ["f?", "f=", "f~", "f^", "f$", "d*x"] {
            assert_eq!(declared(kind), None, "{kind}");
        }
    }
}
