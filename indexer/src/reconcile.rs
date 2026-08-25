//! Index reconciliation — drop rows for snapshots that no longer exist on disk.
//!
//! # Why this module exists (`bd DAS-Backup-Manager-cu8`)
//!
//! `db.rs` was append-and-query only: nothing ever deleted a row. Every snapshot
//! reaped by btrbk retention, or removed by hand, left its `snapshots`/`spans`/`files`
//! rows behind permanently. Measured on the production index 2026-08-24:
//! `/mnt/backup-22tb/projects/` held 2871 indexed snapshots against 327 actually on
//! disk — 2544 dangling, 89%. `btrdasd search` served those paths as current, so a
//! restore chosen from search output could name a snapshot deleted weeks earlier.
//!
//! # The trap this module is built around
//!
//! The obvious implementation — "path missing ⇒ prune" — **destroys the entire
//! index**. DAS targets are deliberately unmounted between backup runs, so every
//! indexed path under `/mnt/backup-22tb/...` is absent for most of the day. A pass
//! run at the wrong moment would conclude that every snapshot ever taken had
//! vanished.
//!
//! The guard is positive rather than negative: a snapshot is a prune candidate
//! **only** when its target root is a verified mountpoint. Anything under a root
//! that is not currently mounted — or under no recognised root at all — is skipped
//! and counted, never pruned. This is the same "a bare mountpoint falls through to
//! the parent filesystem" hazard already documented for backup targets
//! (`.claude/rules/backup.md`, `bd DAS-Backup-Manager-9on`) and on the source side
//! in `doctor.rs`.

use std::path::Path;

use crate::db::Snapshot;
use crate::health::is_mountpoint;

/// Rows removed by a prune pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneStats {
    pub snapshots_removed: usize,
    pub spans_removed: usize,
    pub spans_repaired: usize,
    pub files_removed: usize,
}

/// What a reconcile pass intends to do, before any row is touched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    /// Snapshot ids confirmed absent from a mounted target.
    pub doomed: Vec<i64>,
    /// Snapshots left alone because their root was not a verified mountpoint,
    /// or was not a recognised target root at all. The two are deliberately not
    /// distinguished: both mean "we cannot tell whether this path is really gone",
    /// and both must therefore be skipped.
    pub skipped_unmounted: usize,
    /// Snapshots whose path was confirmed present.
    pub confirmed_present: usize,
}

impl ReconcilePlan {
    /// Whether this plan would change anything.
    pub fn is_empty(&self) -> bool {
        self.doomed.is_empty()
    }
}

/// The mounted root that contains `path`, if any.
///
/// Matching requires a `/` boundary so `/mnt/backup-22` never captures a path
/// under `/mnt/backup-22tb`.
pub fn root_for<'a>(path: &str, roots: &'a [String]) -> Option<&'a str> {
    roots
        .iter()
        .filter(|r| {
            let r = r.trim_end_matches('/');
            path.starts_with(r) && path.as_bytes().get(r.len()) == Some(&b'/')
        })
        // Longest match wins, so nested roots resolve to the more specific one.
        .max_by_key(|r| r.len())
        .map(|r| r.as_str())
}

/// Decide which indexed snapshots are gone, without touching the database.
///
/// `mounted_roots` must contain only roots the caller has *verified* are real
/// mountpoints. `exists` reports whether a snapshot path is present on disk;
/// it is injected so the decision logic is testable with no filesystem.
pub fn plan_reconcile<E>(
    snapshots: &[Snapshot],
    mounted_roots: &[String],
    exists: E,
) -> ReconcilePlan
where
    E: Fn(&str) -> bool,
{
    let mut plan = ReconcilePlan::default();

    for snap in snapshots {
        match root_for(&snap.path, mounted_roots) {
            None => {
                // Either the target is unmounted right now, or this path predates
                // the current target set. Both are "we cannot tell" — never prune.
                plan.skipped_unmounted += 1;
            }
            Some(_) if exists(&snap.path) => plan.confirmed_present += 1,
            Some(_) => plan.doomed.push(snap.id),
        }
    }

    plan
}

/// Filter configured target roots down to those that are genuinely mounted.
pub fn verified_mounted_roots(target_roots: &[String]) -> Vec<String> {
    target_roots
        .iter()
        .filter(|r| is_mountpoint(Path::new(r)))
        .cloned()
        .collect()
}

/// Real-filesystem existence check used by the non-test driver.
pub fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: i64, path: &str) -> Snapshot {
        Snapshot {
            id,
            name: "series".to_string(),
            ts: "20260824T0300".to_string(),
            source: "projects".to_string(),
            path: path.to_string(),
            indexed_at: 0,
        }
    }

    #[test]
    fn root_for_requires_a_path_boundary() {
        let roots = vec!["/mnt/backup-22".to_string()];
        // Prefix-only match must NOT count — this is the bug that would let a
        // pass scoped to one target prune rows belonging to another.
        assert_eq!(
            root_for("/mnt/backup-22tb/projects/x.20260824", &roots),
            None
        );
        assert_eq!(
            root_for("/mnt/backup-22/projects/x.20260824", &roots),
            Some("/mnt/backup-22")
        );
    }

    #[test]
    fn root_for_prefers_the_longest_match() {
        let roots = vec!["/mnt".to_string(), "/mnt/backup-22tb".to_string()];
        assert_eq!(
            root_for("/mnt/backup-22tb/projects/x", &roots),
            Some("/mnt/backup-22tb")
        );
    }

    #[test]
    fn unmounted_target_is_never_pruned() {
        // THE load-bearing test. Targets are unmounted between runs; every path is
        // absent. With no mounted roots, nothing may be doomed.
        let snaps = vec![
            snap(1, "/mnt/backup-22tb/projects/a.20260824"),
            snap(2, "/mnt/backup-22tb/projects/b.20260824"),
        ];
        let plan = plan_reconcile(&snaps, &[], |_| false);
        assert!(plan.doomed.is_empty());
        assert_eq!(plan.skipped_unmounted, 2);
    }

    #[test]
    fn absent_path_on_mounted_root_is_doomed() {
        let snaps = vec![
            snap(1, "/mnt/backup-22tb/projects/gone.20260824"),
            snap(2, "/mnt/backup-22tb/projects/here.20260824"),
        ];
        let roots = vec!["/mnt/backup-22tb".to_string()];
        let plan = plan_reconcile(&snaps, &roots, |p| p.ends_with("here.20260824"));
        assert_eq!(plan.doomed, vec![1]);
        assert_eq!(plan.confirmed_present, 1);
        assert_eq!(plan.skipped_unmounted, 0);
    }

    #[test]
    fn snapshot_under_an_unmounted_root_is_skipped_while_another_target_is_pruned() {
        // Mixed state: one target mounted, one not. The unmounted one must be
        // untouched even though its paths are equally absent.
        let snaps = vec![
            snap(1, "/mnt/backup-22tb/projects/gone.20260824"),
            snap(2, "/mnt/backup-system-recovery-A/projects/gone.20260824"),
        ];
        let roots = vec!["/mnt/backup-22tb".to_string()];
        let plan = plan_reconcile(&snaps, &roots, |_| false);
        assert_eq!(plan.doomed, vec![1]);
        assert_eq!(plan.skipped_unmounted, 1);
    }

    #[test]
    fn empty_plan_reports_itself_empty() {
        let plan = plan_reconcile(&[], &[], |_| true);
        assert!(plan.is_empty());
    }

    #[test]
    fn a_plan_with_doomed_snapshots_is_not_empty() {
        // Pairs with the test above: without this, `is_empty` could return a
        // constant `true` and every prune would silently become a no-op.
        let snaps = vec![snap(1, "/mnt/backup-22tb/projects/gone.20260824")];
        let roots = vec!["/mnt/backup-22tb".to_string()];
        let plan = plan_reconcile(&snaps, &roots, |_| false);
        assert!(!plan.is_empty());
    }

    #[test]
    fn verified_mounted_roots_keeps_real_mounts_and_drops_the_rest() {
        // "/" is always a mountpoint; "/etc" exists but is not one; the third
        // does not exist at all. A constant-empty or constant-passthrough
        // implementation fails this.
        let roots = vec![
            "/".to_string(),
            "/etc".to_string(),
            "/nonexistent-xyzzy-12345".to_string(),
        ];
        assert_eq!(verified_mounted_roots(&roots), vec!["/".to_string()]);
    }

    #[test]
    fn path_exists_distinguishes_present_from_absent() {
        assert!(path_exists("/"));
        assert!(!path_exists("/nonexistent-xyzzy-12345"));
    }
}
