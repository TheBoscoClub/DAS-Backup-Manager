//! End-to-end proof that a subvolume created after setup is backed up by the
//! next run, and that a deleted one is retired and later expired, against real
//! BTRFS filesystems on loop devices. `#[ignore]`d and root-gated: the real
//! `btrfs`, `findmnt`, `blkid` and `btrbk` need root.
//!
//! The unit tests script those commands, so they cannot show the one thing that
//! matters: that a subvolume which was never in `config.toml` really lands in
//! `btrbk`'s output once `sync_subvolumes` has run. Each test also carries its
//! own counter-direction (the subvolume is NOT backed up before sync; the
//! snapshots are NOT deleted inside the window; sync refuses an unmounted
//! volume) so a pass cannot come from a rig that backs up everything anyway.
//!
//! Run with:
//!
//! ```text
//! sudo -E cargo test --test subvol_sync_loopback -- --ignored --nocapture --test-threads=1
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use buttered_dasd::adopt::{SyncOutcome, sync_subvolumes};
use buttered_dasd::btrbk_conf::render_btrbk_conf;
use buttered_dasd::config::{Config, Retention, Source, SubvolConfig, Target, TargetRole};
use buttered_dasd::expire::{ExpireOutcome, expire_retired};
use buttered_dasd::fsutil::SystemRunner;
use buttered_dasd::health::is_mountpoint;

fn is_root() -> bool {
    // SAFETY: geteuid() is always safe.
    unsafe { libc::geteuid() == 0 }
}

fn run(cmd: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot execute {cmd}: {e}"));
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

fn must(cmd: &str, args: &[&str]) -> String {
    let (ok, text) = run(cmd, args);
    assert!(ok, "{cmd} {args:?} failed: {text}");
    text
}

/// A loop-backed BTRFS filesystem mounted at its top level.
struct Loopback {
    mount: PathBuf,
    dev: String,
    uuid: String,
}

impl Loopback {
    /// Image files live in `dir`; the filesystem is mounted at `mount`, which
    /// must be an existing empty directory.
    fn new(dir: &Path, name: &str, mount: PathBuf) -> Self {
        let img = dir.join(format!("{name}.img"));
        must("truncate", &["-s", "512M", img.to_str().unwrap()]);
        let dev = must("losetup", &["--find", "--show", img.to_str().unwrap()])
            .trim()
            .to_string();
        must("mkfs.btrfs", &["-q", "-f", &dev]);
        let uuid = must("blkid", &["-s", "UUID", "-o", "value", &dev])
            .trim()
            .to_string();
        assert!(!uuid.is_empty(), "blkid gave no UUID for {dev}");
        must("mount", &[&dev, mount.to_str().unwrap()]);
        Self { mount, dev, uuid }
    }

    fn unmount(&self) {
        must("umount", &[self.mount.to_str().unwrap()]);
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        // Best effort: the test's own result is what matters.
        let _ = run("umount", &["-R", self.mount.to_str().unwrap()]);
        let _ = run("losetup", &["-d", &self.dev]);
    }
}

/// One source volume holding `@data`, one primary target, the config and the
/// btrbk.conf rendered from it — the state `btrdasd setup` leaves behind.
struct Rig {
    // Declared before `dir` so the mounts are torn down before the directory
    // that holds the images is removed.
    src: Loopback,
    tgt: Loopback,
    config_path: PathBuf,
    btrbk_conf: PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

impl Rig {
    fn new() -> Self {
        Self::with_source_mount_in(None)
    }

    /// `source_mount_base` chooses which filesystem holds the source's mount
    /// point directory (default: the temp dir). It matters once the source is
    /// unmounted: the bare directory then sits on whatever filesystem holds it.
    fn with_source_mount_in(source_mount_base: Option<&Path>) -> Self {
        assert!(is_root(), "these tests need root: run them under sudo");
        let dir = tempfile::tempdir().unwrap();
        let src_base = match source_mount_base {
            Some(base) => tempfile::tempdir_in(base).unwrap(),
            None => tempfile::tempdir_in(dir.path()).unwrap(),
        };
        let tgt_mount = dir.path().join("tgt");
        std::fs::create_dir(&tgt_mount).unwrap();
        let src_mount = src_base.path().join("src-mnt");
        std::fs::create_dir(&src_mount).unwrap();
        let src = Loopback::new(dir.path(), "src", src_mount);
        let tgt = Loopback::new(dir.path(), "tgt", tgt_mount);
        let btrbk_conf = dir.path().join("btrbk.conf");
        let config_path = dir.path().join("config.toml");

        let rig = Self {
            src,
            tgt,
            config_path,
            btrbk_conf,
            _dirs: vec![src_base, dir],
        };
        // The source's snapshot directory is a subvolume, as in production,
        // so the rig also shows that sync never adopts it.
        rig.btrfs(&["subvolume", "create", &rig.src_path(".btrbk-snapshots")]);
        rig.btrfs(&["subvolume", "create", &rig.src_path("@data")]);

        let mut cfg = Config::default();
        cfg.general.btrbk_conf = rig.btrbk_conf.to_string_lossy().into_owned();
        cfg.email.enabled = false;
        cfg.sources = vec![Source {
            label: "src".into(),
            volume: rig.src.mount.to_string_lossy().into_owned(),
            subvolumes: vec![SubvolConfig {
                name: "@data".into(),
                ..Default::default()
            }],
            device: format!("UUID={}", rig.src.uuid),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec!["src".into()],
            target_labels: vec![],
        }];
        cfg.targets = vec![Target {
            label: "primary".into(),
            serial: String::new(),
            serials: vec![],
            mount_uuid: Some(rig.tgt.uuid.clone()),
            mount: rig.tgt.mount.to_string_lossy().into_owned(),
            role: TargetRole::Primary,
            retention: Retention {
                daily: 7,
                ..Default::default()
            },
            display_name: "Loopback".into(),
        }];
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        std::fs::write(&rig.btrbk_conf, render_btrbk_conf(&cfg)).unwrap();
        cfg.save(&rig.config_path).unwrap();
        rig.make_target_dirs();
        rig
    }

    fn src_path(&self, rel: &str) -> String {
        self.src.mount.join(rel).to_string_lossy().into_owned()
    }

    fn tgt_path(&self, rel: &str) -> PathBuf {
        self.tgt.mount.join(rel)
    }

    fn btrfs(&self, args: &[&str]) {
        must("btrfs", args);
    }

    fn write_file(&self, rel: &str, bytes: &[u8]) {
        std::fs::write(self.src.mount.join(rel), bytes).unwrap();
    }

    /// What `backup-run.sh`'s `create_target_dirs` does after the config
    /// reload: btrbk refuses a target subdirectory that does not exist.
    fn make_target_dirs(&self) {
        let cfg = Config::load(&self.config_path).unwrap();
        for source in &cfg.sources {
            let subdir = source.target_subdirs.first().unwrap_or(&source.label);
            std::fs::create_dir_all(self.tgt.mount.join(subdir)).unwrap();
        }
    }

    fn sync(&self, today: &str) -> SyncOutcome {
        let outcome = sync_subvolumes(
            &self.config_path,
            false,
            today,
            &SystemRunner,
            &is_mountpoint,
        )
        .expect("config must load");
        self.make_target_dirs();
        outcome
    }

    fn expire(&self, today: &str) -> ExpireOutcome {
        expire_retired(
            &self.config_path,
            false,
            today,
            &SystemRunner,
            &is_mountpoint,
        )
        .expect("config must load")
    }

    /// `btrbk run` without asserting on the exit status: (success, output).
    fn btrbk_try(&self) -> (bool, String) {
        run("btrbk", &["-c", self.btrbk_conf.to_str().unwrap(), "run"])
    }

    /// Directory names inside the source's snapshot directory, sorted.
    fn source_side_snapshots(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.src.mount.join(".btrbk-snapshots"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn btrbk_run(&self) {
        let out = must("btrbk", &["-c", self.btrbk_conf.to_str().unwrap(), "run"]);
        eprintln!("btrbk run: {} line(s) of output", out.lines().count());
    }

    /// Directory names inside `<target>/<subdir>`, sorted. Empty if the
    /// directory is absent.
    fn target_snapshots(&self, subdir: &str) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(self.tgt_path(subdir)) else {
            return Vec::new();
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

#[test]
#[ignore = "requires root and loop devices"]
fn a_subvolume_created_after_setup_is_backed_up_by_the_next_run() {
    let rig = Rig::new();
    rig.btrfs(&["subvolume", "create", &rig.src_path("@data/nested")]);
    rig.btrfs(&["subvolume", "create", &rig.src_path("brand-new")]);
    rig.write_file("@data/nested/file", b"payload");
    rig.write_file("brand-new/file", b"top-level payload");

    // Counter-test first: btrbk on the config as it stands does not carry them.
    rig.btrbk_run();
    let before = rig.target_snapshots("src");
    assert!(
        !before.is_empty() && before.iter().all(|n| n.starts_with("data.")),
        "before sync only @data may be backed up: {before:?}"
    );
    assert!(
        !rig.tgt_path("src-adopted").exists(),
        "the adoption target directory must not exist before sync"
    );

    let outcome = rig.sync("2026-10-02");
    assert!(outcome.written && !outcome.failed(), "{outcome:?}");
    assert_eq!(outcome.plan.adopt.len(), 2, "{:?}", outcome.plan);
    assert!(
        outcome.plan.retire.is_empty(),
        "nothing existing may be retired: {:?}",
        outcome.plan
    );
    rig.btrbk_run();

    let nested = rig.target_snapshots("src");
    assert!(
        nested.iter().any(|n| n.starts_with("data-nested.")),
        "is on the target after sync: {nested:?}"
    );
    let adopted = rig.target_snapshots("src-adopted");
    assert!(
        adopted.iter().any(|n| n.starts_with("brand-new.")),
        "is on the target after sync: {adopted:?}"
    );

    // The files inside really arrived, not just empty subvolumes.
    let snap = nested
        .iter()
        .find(|n| n.starts_with("data-nested."))
        .unwrap();
    assert_eq!(
        std::fs::read(rig.tgt_path(&format!("src/{snap}/file"))).unwrap(),
        b"payload"
    );
    let snap = adopted
        .iter()
        .find(|n| n.starts_with("brand-new."))
        .unwrap();
    assert_eq!(
        std::fs::read(rig.tgt_path(&format!("src-adopted/{snap}/file"))).unwrap(),
        b"top-level payload"
    );

    // The snapshot directory, a real subvolume on this volume, was never adopted.
    let cfg = Config::load(&rig.config_path).unwrap();
    assert!(
        cfg.sources
            .iter()
            .flat_map(|s| &s.subvolumes)
            .all(|e| !e.name.contains(".btrbk-snapshots")),
        "the snapshot directory must not be adopted"
    );
    let adopted_entries: Vec<&SubvolConfig> = cfg
        .sources
        .iter()
        .flat_map(|s| &s.subvolumes)
        .filter(|e| e.adopted.is_some())
        .collect();
    assert_eq!(adopted_entries.len(), 2, "{adopted_entries:?}");
}

#[test]
#[ignore = "requires root and loop devices"]
fn a_deleted_subvolume_is_retired_and_its_backups_expire_after_the_window() {
    let rig = Rig::new();
    // `keeper` sorts after `doomed`, so the entry that is retired is not the
    // last one in the config (the last entry is never removed).
    rig.btrfs(&["subvolume", "create", &rig.src_path("doomed")]);
    rig.btrfs(&["subvolume", "create", &rig.src_path("keeper")]);
    let outcome = rig.sync("2026-10-02");
    assert!(outcome.written && !outcome.failed(), "{outcome:?}");
    rig.btrbk_run();
    let snaps = rig.target_snapshots("src-adopted");
    assert!(snaps.iter().any(|n| n.starts_with("doomed.")), "{snaps:?}");
    assert!(snaps.iter().any(|n| n.starts_with("keeper.")), "{snaps:?}");

    rig.btrfs(&["subvolume", "delete", &rig.src_path("doomed")]);
    // Counter-direction: with the dead entry still in btrbk.conf, btrbk
    // fails (it exits 10 on a subvolume that no longer exists). This is what
    // retirement exists to prevent.
    let (ok, text) = rig.btrbk_try();
    assert!(
        !ok,
        "btrbk must fail while the dead entry is still configured: {text}"
    );
    assert!(
        std::fs::read_to_string(&rig.btrbk_conf)
            .unwrap()
            .contains("doomed")
    );
    let outcome = rig.sync("2026-10-03");
    assert!(outcome.written && !outcome.failed(), "{outcome:?}");
    assert_eq!(outcome.plan.retire.len(), 1, "{:?}", outcome.plan);
    assert_eq!(outcome.plan.retire[0].name, "doomed");
    assert!(
        !std::fs::read_to_string(&rig.btrbk_conf)
            .unwrap()
            .contains("doomed"),
        "the regenerated btrbk.conf must no longer name the retired subvolume"
    );
    // Now btrbk must not fail on it (`must` asserts exit 0).
    rig.btrbk_run();

    // Inside the 7-day window (retired 2026-10-03, kept through 2026-10-10).
    let early = rig.expire("2026-10-10");
    assert!(!early.failed());
    assert!(
        early.deleted_paths().is_empty(),
        "nothing may be deleted inside the window: {:?}",
        early.deleted_paths()
    );
    let kept = rig.target_snapshots("src-adopted");
    assert!(
        kept.iter().any(|n| n.starts_with("doomed.")),
        "is kept inside the window: {kept:?}"
    );
    let cfg = Config::load(&rig.config_path).unwrap();
    assert!(
        cfg.sources
            .iter()
            .flat_map(|s| &s.subvolumes)
            .any(|e| e.name == "doomed" && e.retired.is_some()),
        "the retired entry stays in the config inside the window"
    );

    assert!(
        rig.source_side_snapshots()
            .iter()
            .any(|n| n.starts_with("doomed.")),
        "source-side doomed.* must exist before the window passes: {:?}",
        rig.source_side_snapshots()
    );

    // Past it: gone from the target and from the source side, and the entry
    // with them. The live neighbour is untouched.
    let late = rig.expire("2026-10-11");
    assert!(!late.failed(), "{:?}", late.entries);
    assert!(!late.deleted_paths().is_empty());
    let left = rig.target_snapshots("src-adopted");
    assert!(
        !left.iter().any(|n| n.starts_with("doomed.")),
        "is deleted after the window: {left:?}"
    );
    assert!(left.iter().any(|n| n.starts_with("keeper.")), "{left:?}");
    let source_side = rig.source_side_snapshots();
    assert!(
        !source_side.iter().any(|n| n.starts_with("doomed.")),
        "source-side snapshots expire too: {source_side:?}"
    );
    let cfg = Config::load(&rig.config_path).unwrap();
    assert!(
        cfg.sources
            .iter()
            .all(|s| s.subvolumes.iter().all(|e| e.name != "doomed")),
        "the entry leaves the config once nothing of it is left"
    );
    // And btrbk is still happy with the config as it now stands.
    rig.btrbk_run();
}

// What this detects: loss of the WHOLE defence. The refusal has four layers in
// `adopt::list_volume` (mountpoint, filesystem UUID, top-level mount, a
// successful non-empty `btrfs subvolume list`), and any one of them alone
// refuses this case, so this test only goes red when the mountpoint, UUID and
// top-level checks are all removed (verified on the VM). Each layer is pinned
// individually by the unit tests in `adopt.rs`:
// `list_volume_refuses_an_unmounted_path_without_running_anything`,
// `wrong_uuid_volume_is_not_listed`,
// `list_volume_refuses_a_volume_not_mounted_at_its_top_level`,
// `list_volume_refuses_when_findmnt_names_no_filesystem_root` and
// `empty_listing_is_a_failed_listing` (each goes red when only its own layer
// is removed, checked on a scratch copy).
#[test]
#[ignore = "requires root and loop devices"]
fn sync_refuses_an_unmounted_source_volume_and_changes_nothing() {
    // The trap is real only if the bare directory sits on a BTRFS filesystem:
    // `btrfs subvolume list` on it then SUCCEEDS, answering for that
    // filesystem instead of failing. So put the mount point on the VM's own
    // BTRFS root, and refuse to run the test if that is not what it is.
    let base = Path::new("/var/lib");
    let fstype = must("stat", &["-f", "-c", "%T", base.to_str().unwrap()]);
    assert_eq!(
        fstype.trim(),
        "btrfs",
        "this test needs {base:?} on BTRFS to show the bare-mountpoint trap"
    );
    let rig = Rig::with_source_mount_in(Some(base));

    // Counter-direction: while mounted, the same call is healthy.
    let healthy = rig.sync("2026-10-02");
    assert!(!healthy.failed() && !healthy.written, "{healthy:?}");

    let before = std::fs::read(&rig.config_path).unwrap();
    let btrbk_before = std::fs::read(&rig.btrbk_conf).unwrap();
    rig.src.unmount();
    // The path is now a bare directory on the VM's own BTRFS root. Prove the
    // trap is live: asked directly, btrfs answers for the WRONG filesystem
    // and succeeds, and what it lists does not contain `@data`.
    assert!(!is_mountpoint(&rig.src.mount));
    let wrong = must(
        "btrfs",
        &["subvolume", "list", rig.src.mount.to_str().unwrap()],
    );
    assert!(
        !wrong.contains("@data"),
        "the bare directory must show another filesystem: {wrong}"
    );

    let outcome = rig.sync("2026-10-03");
    assert!(outcome.plan.failed(), "{outcome:?}");
    assert!(outcome.failed());
    assert!(!outcome.written, "{outcome:?}");
    assert_eq!(outcome.plan.failed_volumes.len(), 1, "{:?}", outcome.plan);
    assert!(
        outcome.plan.retire.is_empty() && outcome.plan.adopt.is_empty(),
        "an unreadable volume must retire and adopt nothing: {:?}",
        outcome.plan
    );
    assert_eq!(
        std::fs::read(&rig.config_path).unwrap(),
        before,
        "config.toml must be byte-identical"
    );
    assert_eq!(
        std::fs::read(&rig.btrbk_conf).unwrap(),
        btrbk_before,
        "btrbk.conf must be byte-identical"
    );
}
