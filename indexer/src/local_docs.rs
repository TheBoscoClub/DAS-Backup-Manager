//! The installation's own recovery documents (`[recovery_os].local_docs_dir`):
//! lists in them that the configuration decides, kept current (bd
//! DAS-Backup-Manager-o77b).
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
//! Fails closed: a file whose markers do not pair (an open with no close, an
//! open inside another, a close with no open), name an unknown attribute or
//! source, or select no entry, is left exactly as it was and named. Markers
//! inside fenced code (```` ``` ```` or `~~~`) are text, never markers, so a
//! guide can show the syntax. Only regular `*.md` files directly in the
//! directory are read; a symlink is refused, never followed.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;

const OPEN: &str = "<!-- btrdasd:subvolumes";
const CLOSE: &str = "<!-- /btrdasd:subvolumes -->";
const END: &str = "-->";

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

/// The `source` and `under` of an opening marker's line (trimmed).
fn attributes(line: &str) -> Result<(String, Option<String>), String> {
    let inner = line
        .strip_prefix(OPEN)
        .and_then(|r| r.strip_suffix(END))
        .ok_or_else(|| format!("{line:?}: a marker opens and ends with --> on one line"))?;
    let (mut source, mut under) = (None, None);
    for word in inner.split_whitespace() {
        let (k, v) = word
            .split_once('=')
            .filter(|(_, v)| !v.is_empty())
            .ok_or_else(|| format!("{line:?}: {word:?} is not key=value"))?;
        let slot = match k {
            "source" => &mut source,
            "under" => &mut under,
            _ => {
                return Err(format!(
                    "{line:?}: {k:?} is not an attribute (source=, under=)"
                ));
            }
        };
        if slot.replace(v.to_string()).is_some() {
            return Err(format!("{line:?}: {k:?} given twice"));
        }
    }
    let source = source.ok_or_else(|| format!("{line:?}: no source= named"))?;
    Ok((source, under))
}

/// `text` with every marked block filled from `config`; `Ok(None)` when
/// nothing would change.
pub fn refreshed(config: &Config, text: &str) -> Result<Option<String>, String> {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<char> = None;
    // The open line's number and the block's new text, while inside one.
    let mut block: Option<(usize, String)> = None;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let n = i + 1;
        let t = line.trim();
        if let Some((at, fill)) = &block {
            if t.starts_with(OPEN) {
                return Err(format!(
                    "line {n}: a marker opens inside another (line {at})"
                ));
            }
            if t == CLOSE {
                out.push_str(fill);
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
        } else if t.starts_with(OPEN) {
            let (source, under) = attributes(t).map_err(|e| format!("line {n}: {e}"))?;
            let list =
                render(config, &source, under.as_deref()).map_err(|e| format!("line {n}: {e}"))?;
            let eol = if line.ends_with("\r\n") { "\r\n" } else { "\n" };
            block = Some((n, format!("{list}{eol}")));
        } else if t == CLOSE {
            return Err(format!("line {n}: a closing marker with no opening one"));
        }
        out.push_str(line);
    }
    if let Some((at, _)) = block {
        return Err(format!("line {at}: a marker with no closing {CLOSE}"));
    }
    Ok((out != text).then_some(out))
}

/// Refresh every regular `*.md` directly in `dir`; with `write` false, only
/// say what would change. A directory that does not exist has no documents.
pub fn refresh_dir(config: &Config, dir: &Path, write: bool) -> Result<Vec<DocReport>, String> {
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
            let outcome = refresh_one(config, &path, write);
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

fn refresh_one(config: &Config, path: &Path, write: bool) -> DocOutcome {
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
    match refreshed(config, &text) {
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
    use crate::config::{Config, Source, SubvolConfig};

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
        let out = refreshed(&config(), &doc("stale, list")).unwrap().unwrap();
        assert_eq!(out, doc("Alpha, beta"));
    }

    #[test]
    fn without_under_every_entry_by_its_full_name() {
        let text = "<!-- btrdasd:subvolumes source=projects -->\nx\n<!-- /btrdasd:subvolumes -->\n";
        let out = refreshed(&config(), text).unwrap().unwrap();
        assert_eq!(
            out,
            "<!-- btrdasd:subvolumes source=projects -->\nOther, Work, Work/Alpha, Work/beta, Work/beta/nested\n<!-- /btrdasd:subvolumes -->\n"
        );
    }

    #[test]
    fn a_current_block_and_a_file_without_markers_are_left_alone() {
        assert_eq!(refreshed(&config(), &doc("Alpha, beta")).unwrap(), None);
        assert_eq!(refreshed(&config(), "# Nothing marked\n").unwrap(), None);
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
            let err = refreshed(&config(), &text).expect_err(&text);
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }

    #[test]
    fn markers_inside_fenced_code_are_text() {
        let text = format!("```text\n{OPEN_LINE}\nexample\n```\n~~~\n{CLOSE}\n~~~\n");
        assert_eq!(refreshed(&config(), &text).unwrap(), None);
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

        let checked = refresh_dir(&config(), p, false).unwrap();
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
        let done = refresh_dir(&config(), p, true).unwrap();
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
            refresh_dir(&config(), &p.join("absent"), true)
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
}
