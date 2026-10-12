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
use crate::maintenance::{HoldsMaintenance, MaintenanceHeld};
use crate::scrub;

/// Singleton lock for a standalone reconcile pass.
///
/// A standalone `btrdasd reconcile` mounts and unmounts the DAS targets, so it
/// falls under the same maintenance interlock as backups and scrubs
/// (`.claude/rules/backup.md`). It takes the singleton lock and then the shared
/// maintenance lock, in that order — matching `backup-run.sh`, the scrub engine
/// and `doctor.rs`, which is what keeps the set deadlock-free.
///
/// Both are acquired **non-blocking**, like `doctor.rs` and unlike the scrub
/// engine: an index tidy-up has nothing urgent enough to delay a real backup
/// for, so a held lock defers the pass rather than queuing behind it.
///
/// The reconcile performed inside `btrdasd walk` takes no lock of its own: it
/// runs under the maintenance lock `walk` already holds — taken (waiting for
/// it) or, when `backup-run.sh` runs `walk`, handed down by the script
/// (`maintenance::DELEGATED_FD_ENV`).
pub const RECONCILE_LOCK_PATH: &str = "/run/das-reconcile.lock";

/// Both locks a standalone reconcile holds, released on drop — maintenance
/// first (fields drop in declaration order).
pub struct ReconcileLocks {
    maintenance: MaintenanceHeld,
    _singleton: scrub::FileLock,
}

impl HoldsMaintenance for ReconcileLocks {
    fn maintenance(&self) -> &MaintenanceHeld {
        &self.maintenance
    }
}

/// Outcome of trying to acquire the standalone reconcile locks.
pub enum LockAttempt {
    Acquired(Box<ReconcileLocks>),
    /// Another maintenance operation holds a lock — defer, do not queue.
    Deferred(String),
}

/// Try to take the standalone reconcile locks without blocking. `job`
/// (`btrdasd reindex`) is recorded in the maintenance lock file as its
/// holder.
///
/// Split from `try_acquire_locks()` so the acquisition ORDER and the defer
/// behaviour can be tested against temp paths, with no root and no /run writes.
pub fn try_acquire_locks_at(
    singleton_path: &Path,
    maintenance_path: &Path,
    job: &str,
) -> Result<LockAttempt, scrub::ScrubError> {
    let Some(singleton) = scrub::FileLock::try_acquire(singleton_path)? else {
        return Ok(LockAttempt::Deferred(
            "another reconcile is already running".to_string(),
        ));
    };
    let Some(maintenance) = MaintenanceHeld::try_acquire_at(maintenance_path, job)? else {
        return Ok(LockAttempt::Deferred(format!(
            "DAS maintenance lock held by {}",
            crate::maintenance::holder_of(maintenance_path)
        )));
    };
    Ok(LockAttempt::Acquired(Box::new(ReconcileLocks {
        maintenance,
        _singleton: singleton,
    })))
}

/// Try to take the standalone reconcile locks at their production paths.
pub fn try_acquire_locks(job: &str) -> Result<LockAttempt, scrub::ScrubError> {
    try_acquire_locks_at(
        Path::new(RECONCILE_LOCK_PATH),
        Path::new(scrub::MAINTENANCE_LOCK_PATH),
        job,
    )
}

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
    /// Under a CONFIGURED target root that is not mounted right now. Transient:
    /// the next pass with that target mounted will resolve it.
    pub skipped_unmounted: usize,
    /// Under no configured target root at all — typically a mount path retired by
    /// a rename. Permanently unreachable: the root will never be mounted again, so
    /// no reconcile can ever confirm these paths are gone, and they would otherwise
    /// be counted as "unmounted" forever and mask the transient number
    /// (bd DAS-Backup-Manager-wl8). Clearing them needs `btrdasd reindex --rebuild`.
    pub skipped_unknown_root: usize,
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
    configured_roots: &[String],
    exists: E,
) -> ReconcilePlan
where
    E: Fn(&str) -> bool,
{
    let mut plan = ReconcilePlan::default();

    for snap in snapshots {
        match root_for(&snap.path, mounted_roots) {
            Some(_) if exists(&snap.path) => plan.confirmed_present += 1,
            Some(_) => plan.doomed.push(snap.id),
            // Not under a mounted root, so we cannot tell whether it is really
            // gone — never prune. But WHY we cannot tell matters: a configured
            // root that happens to be unmounted resolves itself next pass, while
            // a root no longer in the config never will.
            None if root_for(&snap.path, configured_roots).is_some() => plan.skipped_unmounted += 1,
            None => plan.skipped_unknown_root += 1,
        }
    }

    plan
}

/// What the host says about a mount point: `(filesystem UUID, FSROOT)`, from
/// `findmnt -no UUID,FSROOT <path>`. `None` when it cannot say.
pub type RootProbe<'a> = &'a dyn Fn(&Path) -> Option<(String, String)>;

/// The configured target roots a pass may trust.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct VerifiedRoots {
    /// Mount points that are the configured filesystem's top level, plus
    /// those of targets without a `mount_uuid` (see `unchecked`).
    pub roots: Vec<String>,
    /// Labels of mounted targets without a `mount_uuid` (serial-only): their
    /// identity cannot be checked, they are kept as before. For the caller to log.
    pub unchecked: Vec<String>,
    /// `(mount, message)` for each mount point that IS mounted but is not the
    /// configured filesystem's top level. Never walked, never reconciled.
    pub refused: Vec<(String, String)>,
}

impl VerifiedRoots {
    /// The refusal message when `path` is a refused mount point.
    pub fn refusal_for(&self, path: &Path) -> Option<&str> {
        let want = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.refused.iter().find_map(|(mount, why)| {
            let m = Path::new(mount);
            let m = m.canonicalize().unwrap_or_else(|_| m.to_path_buf());
            (m == want).then_some(why.as_str())
        })
    }
}

/// Filter configured targets down to the roots that are genuinely mounted AND
/// are the configured filesystem's top level.
///
/// A mount point is not an identity: on a booted recovery drive the
/// configured path can hold a bind of the running `/` (subvolume `@`), under
/// which every recorded snapshot path is "absent" and a prune would delete the
/// drive's own rows. For a target with a `mount_uuid`, `probe` must therefore
/// report that UUID (case-insensitive) and FSROOT `/`; anything else —
/// including a probe that cannot answer — is refused (the cautious reading).
pub fn verify_target_roots(
    targets: &[crate::config::Target],
    is_mount: &dyn Fn(&Path) -> bool,
    probe: RootProbe<'_>,
) -> VerifiedRoots {
    let mut out = VerifiedRoots::default();
    for t in targets {
        if !is_mount(Path::new(&t.mount)) {
            continue;
        }
        let Some(want) = t.mount_uuid.as_deref().filter(|u| !u.is_empty()) else {
            out.unchecked.push(t.label.clone());
            out.roots.push(t.mount.clone());
            continue;
        };
        let seen = probe(Path::new(&t.mount));
        match &seen {
            Some((uuid, fsroot)) if uuid.eq_ignore_ascii_case(want) && fsroot == "/" => {
                out.roots.push(t.mount.clone());
            }
            _ => {
                let (uuid, fsroot) = match &seen {
                    Some((u, r)) => (u.as_str(), r.as_str()),
                    None => ("unknown", "unknown"),
                };
                out.refused.push((
                    t.mount.clone(),
                    format!(
                        "{}: mounted but not the configured filesystem's top level \
                         (uuid {uuid}, fsroot {fsroot}) — skipped",
                        t.label
                    ),
                ));
            }
        }
    }
    out
}

/// What `reconcile --forget-root` did with the root it was given.
#[derive(Debug, PartialEq, Eq)]
pub enum RootRetirement {
    /// The root is a configured target: nothing was touched, and the command
    /// has failed — dropping its rows would discard a live index.
    Refused(String),
    /// No index rows sit under the root.
    NoRows,
    /// Dry run: this many rows would be dropped.
    WouldDrop(usize),
    Dropped(PruneStats),
}

/// Retire the index rows under `root`, a mount path no longer configured.
/// A root that IS one of `configured_mounts` is refused, untouched.
pub fn retire_root(
    database: &crate::db::Database,
    configured_mounts: &[String],
    root: &str,
    dry_run: bool,
) -> rusqlite::Result<RootRetirement> {
    if configured_mounts
        .iter()
        .any(|c| c.trim_end_matches('/') == root.trim_end_matches('/'))
    {
        return Ok(RootRetirement::Refused(format!(
            "refusing: {root} IS a configured target — reconcile handles it \
             normally, and dropping its rows would discard a live index"
        )));
    }
    let ids = database.snapshots_under_root(root)?;
    Ok(if ids.is_empty() {
        RootRetirement::NoRows
    } else if dry_run {
        RootRetirement::WouldDrop(ids.len())
    } else {
        RootRetirement::Dropped(database.prune_snapshots(&ids)?)
    })
}

/// The error a reconcile pass ends with when something it was asked to do
/// failed (a refused `--forget-root`, a failed `--repair`), or `None`.
pub fn failure_error(failures: &[String]) -> Option<String> {
    (!failures.is_empty()).then(|| failures.join("; "))
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
    fn retiring_a_configured_root_is_refused_and_touches_nothing() {
        let db = crate::db::Database::open(":memory:").unwrap();
        db.insert_snapshot("a", "20260101T0300", "s", "/mnt/t/s/a.20260101T0300")
            .unwrap();
        let configured = vec!["/mnt/t/".to_string()];
        let got = retire_root(&db, &configured, "/mnt/t", false).unwrap();
        assert!(matches!(got, RootRetirement::Refused(_)), "{got:?}");
        assert_eq!(db.list_snapshots().unwrap().len(), 1);
        // The counter-case: an unconfigured root is dropped.
        let got = retire_root(&db, &[], "/mnt/t", false).unwrap();
        assert!(matches!(got, RootRetirement::Dropped(_)), "{got:?}");
        assert!(db.list_snapshots().unwrap().is_empty());
    }

    #[test]
    fn failures_join_into_one_error_and_none_is_no_error() {
        assert_eq!(failure_error(&[]), None);
        assert_eq!(
            failure_error(&["a".to_string(), "b".to_string()]),
            Some("a; b".to_string())
        );
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
        // Both paths are under a CONFIGURED root that simply is not mounted, so
        // they count as transient-unmounted rather than permanently unreachable.
        let roots_cfg = vec!["/mnt/backup-22tb".to_string()];
        let plan = plan_reconcile(&snaps, &[], &roots_cfg, |_| false);
        assert!(plan.doomed.is_empty());
        assert_eq!(plan.skipped_unmounted, 2);
        assert_eq!(plan.skipped_unknown_root, 0);
    }

    #[test]
    fn absent_path_on_mounted_root_is_doomed() {
        let snaps = vec![
            snap(1, "/mnt/backup-22tb/projects/gone.20260824"),
            snap(2, "/mnt/backup-22tb/projects/here.20260824"),
        ];
        let roots = vec!["/mnt/backup-22tb".to_string()];
        let plan = plan_reconcile(&snaps, &roots, &roots, |p| p.ends_with("here.20260824"));
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
        // Both targets are configured; only one is mounted. The unmounted one is
        // transient, NOT a retired root.
        let mounted = vec!["/mnt/backup-22tb".to_string()];
        let configured = vec![
            "/mnt/backup-22tb".to_string(),
            "/mnt/backup-system-recovery-A".to_string(),
        ];
        let plan = plan_reconcile(&snaps, &mounted, &configured, |_| false);
        assert_eq!(plan.doomed, vec![1]);
        assert_eq!(plan.skipped_unmounted, 1);
        assert_eq!(plan.skipped_unknown_root, 0);
    }

    #[test]
    fn a_retired_mount_root_is_counted_separately_from_a_merely_unmounted_one() {
        // bd DAS-Backup-Manager-wl8: rows under a root retired by a rename can
        // never be confirmed gone, because that root will never be mounted again.
        // Folding them into "unmounted" left a count that never reached zero and
        // so stopped being a signal.
        let snaps = vec![
            snap(1, "/mnt/backup-22tb/projects/a.20260824"), // configured, not mounted
            snap(2, "/mnt/backup-system-mirror/projects/b.20260824"), // retired root
            snap(3, "/mnt/backup-system/nvme/c.20260824"),   // retired root
        ];
        let configured = vec!["/mnt/backup-22tb".to_string()];
        let plan = plan_reconcile(&snaps, &[], &configured, |_| false);

        assert!(
            plan.doomed.is_empty(),
            "nothing may be pruned with no mounted root"
        );
        assert_eq!(plan.skipped_unmounted, 1);
        assert_eq!(plan.skipped_unknown_root, 2);
    }

    #[test]
    fn empty_plan_reports_itself_empty() {
        let plan = plan_reconcile(&[], &[], &[], |_| true);
        assert!(plan.is_empty());
    }

    #[test]
    fn a_plan_with_doomed_snapshots_is_not_empty() {
        // Pairs with the test above: without this, `is_empty` could return a
        // constant `true` and every prune would silently become a no-op.
        let snaps = vec![snap(1, "/mnt/backup-22tb/projects/gone.20260824")];
        let roots = vec!["/mnt/backup-22tb".to_string()];
        let plan = plan_reconcile(&snaps, &roots, &roots, |_| false);
        assert!(!plan.is_empty());
    }

    fn tgt(label: &str, mount: &str, uuid: Option<&str>) -> crate::config::Target {
        crate::config::Target {
            label: label.to_string(),
            serial: String::new(),
            serials: vec![],
            mount_uuid: uuid.map(str::to_string),
            mount: mount.to_string(),
            role: crate::config::TargetRole::Primary,
            retention: crate::config::Retention {
                weekly: 4,
                monthly: 2,
                daily: 7,
                yearly: 1,
            },
            display_name: String::new(),
        }
    }

    const U: &str = "5eed0008-1111-2222-3333-444444444444";

    fn answers(uuid: &str, fsroot: &str) -> impl Fn(&Path) -> Option<(String, String)> {
        let (u, r) = (uuid.to_string(), fsroot.to_string());
        move |_| Some((u.clone(), r.clone()))
    }

    #[test]
    fn a_mount_that_is_the_configured_top_level_is_verified() {
        let t = [tgt("rec-A", "/mnt/a", Some(U))];
        // Upper-case UUID from the host: matched case-insensitively.
        let v = verify_target_roots(&t, &|_| true, &answers(&U.to_uppercase(), "/"));
        assert_eq!(v.roots, vec!["/mnt/a".to_string()]);
        assert!(v.refused.is_empty() && v.unchecked.is_empty());
    }

    #[test]
    fn a_mount_of_another_filesystem_is_refused_with_its_identity() {
        let t = [tgt("rec-A", "/mnt/a", Some(U))];
        let v = verify_target_roots(&t, &|_| true, &answers("dead-beef", "/"));
        assert!(v.roots.is_empty());
        assert_eq!(
            v.refused[0].1,
            "rec-A: mounted but not the configured filesystem's top level \
             (uuid dead-beef, fsroot /) — skipped"
        );
    }

    #[test]
    fn a_subvolume_mount_of_the_right_filesystem_is_refused() {
        // The bind of the booted drive's `/` shows its own UUID, FSROOT /@.
        let t = [tgt("rec-A", "/mnt/a", Some(U))];
        let v = verify_target_roots(&t, &|_| true, &answers(U, "/@"));
        assert!(v.roots.is_empty());
        assert!(v.refused[0].1.contains(&format!("(uuid {U}, fsroot /@)")));
        assert_eq!(
            v.refusal_for(Path::new("/mnt/a")),
            Some(v.refused[0].1.as_str())
        );
        assert_eq!(v.refusal_for(Path::new("/mnt/other")), None);
    }

    #[test]
    fn a_probe_that_cannot_answer_refuses_the_root() {
        let t = [tgt("rec-A", "/mnt/a", Some(U))];
        let v = verify_target_roots(&t, &|_| true, &|_| None);
        assert!(v.roots.is_empty());
        assert!(v.refused[0].1.contains("(uuid unknown, fsroot unknown)"));
    }

    #[test]
    fn an_unmounted_target_is_neither_verified_nor_refused_and_is_not_probed() {
        let t = [tgt("rec-A", "/mnt/a", Some(U))];
        let probe = |_: &Path| -> Option<(String, String)> { panic!("probed an unmounted root") };
        let v = verify_target_roots(&t, &|_| false, &probe);
        assert_eq!(v, VerifiedRoots::default());
    }

    #[test]
    fn a_target_without_a_uuid_keeps_the_mountpoint_check_and_is_listed() {
        let t = [
            tgt("serial-only", "/mnt/s", None),
            tgt("empty", "/mnt/e", Some("")),
        ];
        let probe = |_: &Path| -> Option<(String, String)> { panic!("no uuid to compare") };
        let v = verify_target_roots(&t, &|_| true, &probe);
        assert_eq!(v.roots, vec!["/mnt/s".to_string(), "/mnt/e".to_string()]);
        assert_eq!(
            v.unchecked,
            vec!["serial-only".to_string(), "empty".to_string()]
        );
    }

    #[test]
    fn a_refused_root_is_never_reconciled() {
        // Every recorded path under the refused root is absent, and a pass
        // that trusted the bind would doom them all. Through the same
        // verification the commands use, none is doomed.
        let t = [
            tgt("rec-A", "/mnt/a", Some(U)),
            tgt("ok", "/mnt/ok", Some(U)),
        ];
        let snaps = vec![
            snap(1, "/mnt/a/nvme/root-.1"),
            snap(2, "/mnt/ok/nvme/root-.2"),
        ];
        let roots: Vec<String> = t.iter().map(|x| x.mount.clone()).collect();
        let probe = |p: &Path| {
            Some((
                U.to_string(),
                if p == Path::new("/mnt/a") { "/@" } else { "/" }.to_string(),
            ))
        };
        let v = verify_target_roots(&t, &|_| true, &probe);
        let plan = plan_reconcile(&snaps, &v.roots, &roots, |_| false);
        assert_eq!(plan.doomed, vec![2], "only the verified root's row can go");
        assert_eq!(plan.skipped_unmounted, 1, "the refused root's row is kept");
    }

    #[test]
    fn reconcile_defers_when_the_maintenance_lock_is_held() {
        // THE interlock test: a backup or scrub holding /run/das-maintenance.lock
        // must make a standalone reconcile stand down rather than mount the DAS
        // targets underneath it.
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("reconcile.lock");
        let maintenance = dir.path().join("maintenance.lock");

        let held = MaintenanceHeld::try_acquire_at(&maintenance, "btrdasd scrub run").unwrap();
        assert!(held.is_some(), "fixture must actually hold the lock");

        match try_acquire_locks_at(&singleton, &maintenance, "btrdasd reconcile").unwrap() {
            LockAttempt::Deferred(why) => assert_eq!(
                why,
                format!(
                    "DAS maintenance lock held by btrdasd scrub run pid {}",
                    std::process::id()
                ),
                "the holder is named, never guessed"
            ),
            LockAttempt::Acquired(_) => panic!("acquired while maintenance lock was held"),
        }
    }

    #[test]
    fn reconcile_defers_when_another_reconcile_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("reconcile.lock");
        let maintenance = dir.path().join("maintenance.lock");

        let held = scrub::FileLock::try_acquire(&singleton).unwrap();
        assert!(held.is_some());

        match try_acquire_locks_at(&singleton, &maintenance, "btrdasd reconcile").unwrap() {
            LockAttempt::Deferred(why) => assert!(why.contains("reconcile")),
            LockAttempt::Acquired(_) => panic!("two reconciles acquired at once"),
        }
    }

    #[test]
    fn reconcile_acquires_when_nothing_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let singleton = dir.path().join("reconcile.lock");
        let maintenance = dir.path().join("maintenance.lock");
        match try_acquire_locks_at(&singleton, &maintenance, "btrdasd reindex").unwrap() {
            LockAttempt::Acquired(locks) => {
                assert_eq!(locks.maintenance().path(), maintenance);
                assert_eq!(
                    std::fs::read_to_string(&maintenance).unwrap(),
                    format!("btrdasd reindex pid {}\n", std::process::id()),
                    "the job is recorded as the maintenance lock's holder"
                );
            }
            LockAttempt::Deferred(why) => panic!("deferred with nothing held: {why}"),
        }
    }

    #[test]
    fn path_exists_distinguishes_present_from_absent() {
        assert!(path_exists("/"));
        assert!(!path_exists("/nonexistent-xyzzy-12345"));
    }
}
