//! Keep `config.toml` in step with the subvolumes that actually exist.
//!
//! The backup used to be an allowlist: a subvolume was backed up only if an
//! entry named it, and `btrfs send` does not descend into nested subvolumes,
//! so a missing entry meant a silently empty directory in every snapshot.
//! This module inverts that. See
//! `docs/superpowers/specs/2026-10-01-subvolume-reconcile-design.md`.

use crate::doctor::glob_match;

/// Whether `path` lies in a snapshot tree — Snapper's `.snapshots` or btrbk's
/// `.btrbk-snapshots`, at any depth. Snapshots are not data to back up; they
/// are copies of data that is.
pub fn in_snapshot_tree(path: &str) -> bool {
    path.split('/')
        .any(|component| component == ".snapshots" || component == ".btrbk-snapshots")
}

/// The first exclude pattern that keeps `path` out, if any. A pattern applies
/// to the subvolume it matches and to everything nested under that
/// subvolume, so excluding `@cache` also excludes `@cache/stremio`.
pub fn excluding_pattern<'a>(path: &str, patterns: &'a [String]) -> Option<&'a str> {
    patterns
        .iter()
        .find(|pattern| {
            // The path itself, then each ancestor: "a/b/c", "a/b", "a".
            let mut candidate = path;
            loop {
                if glob_match(pattern, candidate) {
                    return true;
                }
                match candidate.rfind('/') {
                    Some(cut) => candidate = &candidate[..cut],
                    None => return false,
                }
            }
        })
        .map(String::as_str)
}

/// Subvolume paths from `btrfs subvolume list` output. Each line reads
/// `ID 256 gen 100 top level 5 path <path>`; the path is everything after
/// the first ` path `, because a path may itself contain spaces.
pub fn parse_subvolume_paths(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_once(" path "))
        .map(|(_, path)| path.to_string())
        .filter(|path| !path.is_empty())
        .collect()
}

/// What was found on one source volume. `Err` means the volume could not be
/// read and nothing may be concluded about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeListing {
    pub volume: String,
    pub subvolumes: Result<Vec<String>, String>,
}

/// An empty listing is treated as a failed one. Every configured volume holds
/// at least the subvolumes that are being backed up from it, so "nothing
/// here" is the signature of reading the wrong filesystem, and believing it
/// would retire every entry on the volume.
pub fn normalize_listing(volume: &str, listed: Result<Vec<String>, String>) -> VolumeListing {
    let subvolumes = match listed {
        Ok(found) if found.is_empty() => Err(format!(
            "'{volume}' listed no subvolumes — treating the volume as unreadable"
        )),
        other => other,
    };
    VolumeListing {
        volume: volume.to_string(),
        subvolumes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn in_snapshot_tree_matches_a_snapshot_directory_at_any_depth() {
        for inside in [
            ".snapshots",
            ".snapshots/4460/snapshot",
            "@root/.snapshots/3811/snapshot",
            ".btrbk-snapshots/root.20260801T0325",
            "Audiobooks/.btrbk-snapshots/foo.20260101T0000",
        ] {
            assert!(in_snapshot_tree(inside), "{inside}");
        }
        for outside in ["@", "@home", "snapshots", "my.snapshots", "a/.snapshotsx/b"] {
            assert!(!in_snapshot_tree(outside), "{outside}");
        }
    }

    #[test]
    fn excluding_pattern_covers_the_path_and_everything_nested_under_it() {
        let p = pats(&["@cache", "coredumps", "scratch-*"]);
        assert_eq!(excluding_pattern("@cache", &p), Some("@cache"));
        assert_eq!(excluding_pattern("@cache/stremio", &p), Some("@cache"));
        assert_eq!(excluding_pattern("@cache/a/b/c", &p), Some("@cache"));
        assert_eq!(excluding_pattern("scratch-1/tmp", &p), Some("scratch-*"));
        // A sibling that merely starts with the same letters is not nested.
        assert_eq!(excluding_pattern("@cache2", &p), None);
        assert_eq!(excluding_pattern("@cachefoo/x", &p), None);
        // Nesting is downward only: a pattern for a child does not exclude its parent.
        assert_eq!(excluding_pattern("a", &pats(&["a/b"])), None);
        assert_eq!(excluding_pattern("@home", &p), None);
        assert_eq!(excluding_pattern("@home", &[]), None);
    }

    #[test]
    fn excluding_pattern_reports_the_first_pattern_that_applies() {
        let p = pats(&["zzz", "@cache*", "@cache"]);
        assert_eq!(excluding_pattern("@cache/x", &p), Some("@cache*"));
    }

    #[test]
    fn parse_subvolume_paths_keeps_spaces() {
        let out = "ID 256 gen 100 top level 5 path @\n\
                   ID 300 gen 9 top level 257 path bosco-media/My Films\n\
                   ID 301 gen 9 top level 5 path a path b\n\
                   \n\
                   garbage line with no marker\n";
        assert_eq!(
            parse_subvolume_paths(out),
            ["@", "bosco-media/My Films", "a path b"]
        );
    }

    #[test]
    fn empty_listing_is_a_failed_listing() {
        let l = normalize_listing("/vol", Ok(Vec::new()));
        assert_eq!(l.volume, "/vol");
        let why = l.subvolumes.unwrap_err();
        assert!(why.contains("no subvolumes"), "{why}");

        let l = normalize_listing("/vol", Err("boom".into()));
        assert_eq!(l.subvolumes.unwrap_err(), "boom");

        let l = normalize_listing("/vol", Ok(vec!["@".into()]));
        assert_eq!(l.subvolumes.unwrap(), ["@"]);
    }
}
