//! End-to-end `archive_boot` tests against a real BTRFS filesystem on a loop
//! device. `#[ignore]`d and root-gated: they create loop devices, real mounts,
//! and genuine btrfs subvolumes, and they exercise real
//! `btrfs subvolume delete`.
//!
//! These exist because the unit tests cannot prove the property that matters.
//! `archive_boot`'s bug (bd DAS-Backup-Manager-5ig) was that it removed the
//! LIVE subvolume before discovering it had no replacement. A tempdir-based
//! test cannot observe that: `btrfs subvolume delete` fails on a plain
//! directory, so the live path survives either way and the test passes against
//! the broken code. Only a real subvolume distinguishes the two.
//!
//! Run with:
//!
//! ```text
//! sudo -E cargo test --test boot_archive_loopback -- --ignored --nocapture --test-threads=1
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use buttered_dasd::backup::archive_boot;
use buttered_dasd::config::Config;
use buttered_dasd::progress::{LogLevel, ProgressCallback};

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

struct Collector(std::sync::Mutex<Vec<String>>);

impl ProgressCallback for Collector {
    fn on_stage(&self, _: &str, _: u64) {}
    fn on_progress(&self, _: u64, _: u64, _: &str) {}
    fn on_log(&self, level: LogLevel, msg: &str) {
        self.0.lock().unwrap().push(format!("{level:?}: {msg}"));
    }
    fn on_throughput(&self, _: u64) {}
    fn on_complete(&self, _: bool, _: &str) {}
}

impl Collector {
    fn new() -> Self {
        Self(std::sync::Mutex::new(Vec::new()))
    }
    fn dump(&self) -> String {
        self.0.lock().unwrap().join("\n")
    }
}

/// A loop-backed BTRFS filesystem that tears itself down.
struct Loopback {
    dir: tempfile::TempDir,
    mount: PathBuf,
    dev: String,
}

impl Loopback {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        must("truncate", &["-s", "512M", img.to_str().unwrap()]);
        let dev = must("losetup", &["--find", "--show", img.to_str().unwrap()])
            .trim()
            .to_string();
        must("mkfs.btrfs", &["-q", "-f", &dev]);
        let mount = dir.path().join("mnt");
        std::fs::create_dir(&mount).unwrap();
        must("mount", &[&dev, mount.to_str().unwrap()]);
        Self { dir, mount, dev }
    }

    fn subvol(&self, rel: &str) -> PathBuf {
        let p = self.mount.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        must("btrfs", &["subvolume", "create", p.to_str().unwrap()]);
        p
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        let _ = run("umount", &["-R", self.mount.to_str().unwrap()]);
        let _ = run("losetup", &["-d", &self.dev]);
        let _ = &self.dir;
    }
}

/// Config wired to this filesystem, with a btrbk.conf naming `@` -> `name`.
fn config_for(fs: &Loopback, snapshot_name: &str) -> (Config, tempfile::NamedTempFile) {
    use std::io::Write;
    let mut conf = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        conf,
        "volume /.btrfs-nvme\n  subvolume  @\n    snapshot_name  {snapshot_name}\n"
    )
    .unwrap();

    let mut cfg = Config::default();
    cfg.general.btrbk_conf = conf.path().to_string_lossy().into_owned();
    cfg.boot.enabled = true;
    cfg.boot.subvolumes = vec!["@".into()];
    cfg.sources = vec![buttered_dasd::config::Source {
        label: "nvme".into(),
        volume: "/.btrfs-nvme".into(),
        subvolumes: vec![buttered_dasd::config::SubvolConfig {
            name: "@".into(),
            manual_only: false,
            snapshot_name: None,
        }],
        device: "loop".into(),
        snapshot_dir: ".btrbk-snapshots".into(),
        target_subdirs: vec!["nvme".into()],
        target_labels: vec![],
    }];
    cfg.targets = vec![buttered_dasd::config::Target {
        label: "primary".into(),
        serial: "LOOPTEST".into(),
        serials: vec!["LOOPTEST".into()],
        mount_uuid: None,
        mount: fs.mount.to_string_lossy().into_owned(),
        role: buttered_dasd::config::TargetRole::Primary,
        retention: Default::default(),
        display_name: "Loopback".into(),
    }];
    (cfg, conf)
}

fn archives_in(mount: &Path) -> Vec<String> {
    std::fs::read_dir(mount)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("@.archive."))
        .collect()
}

#[test]
#[ignore = "requires root and loop devices"]
fn archive_boot_replaces_the_live_subvolume_when_a_snapshot_exists() {
    if !is_root() {
        eprintln!("skipping: not root");
        return;
    }
    let fs = Loopback::new();
    // A received snapshot under the source's target_subdir, named exactly as
    // production btrbk names it for a disambiguated bare `@`.
    let snap = fs.subvol("nvme/root-.20260827T2015");
    std::fs::write(snap.join("marker"), "from-snapshot").unwrap();
    // The live boot subvolume, with different content.
    let live = fs.subvol("@");
    std::fs::write(live.join("marker"), "outgoing").unwrap();

    let (cfg, _conf) = config_for(&fs, "root-");
    let progress = Collector::new();
    let archived = archive_boot(&cfg, &progress).expect("archive_boot must not error");

    assert!(
        archived,
        "should report having archived: {}",
        progress.dump()
    );
    assert!(live.exists(), "live @ must exist after the run");
    assert_eq!(
        std::fs::read_to_string(live.join("marker")).unwrap(),
        "from-snapshot",
        "live @ must have been rebuilt from the newest snapshot"
    );
    let archives = archives_in(&fs.mount);
    assert_eq!(archives.len(), 1, "exactly one archive: {archives:?}");
    assert_eq!(
        std::fs::read_to_string(fs.mount.join(&archives[0]).join("marker")).unwrap(),
        "outgoing",
        "the archive must hold the OUTGOING contents"
    );
    assert!(
        !fs.mount.join("@.new").exists(),
        "staging subvolume must not be left behind"
    );
}

#[test]
#[ignore = "requires root and loop devices"]
fn archive_boot_never_deletes_the_live_subvolume_without_a_replacement() {
    // THE 5ig REGRESSION. btrbk.conf says `root-`, which matches nothing on
    // this filesystem, so the lookup fails. Pre-0.7.20.0 the live subvolume
    // had already been deleted by this point and the run merely logged a
    // warning about being unable to recreate it.
    if !is_root() {
        eprintln!("skipping: not root");
        return;
    }
    let fs = Loopback::new();
    let live = fs.subvol("@");
    std::fs::write(live.join("marker"), "irreplaceable").unwrap();
    // Deliberately no `nvme/root-.<TS>` snapshot anywhere.

    let (cfg, _conf) = config_for(&fs, "root-");
    let progress = Collector::new();
    let archived = archive_boot(&cfg, &progress).expect("archive_boot must not error");

    assert!(!archived, "nothing should be archived: {}", progress.dump());
    assert!(
        live.exists(),
        "live @ MUST survive when no replacement exists: {}",
        progress.dump()
    );
    assert_eq!(
        std::fs::read_to_string(live.join("marker")).unwrap(),
        "irreplaceable",
        "live @ contents must be untouched"
    );
    assert!(
        archives_in(&fs.mount).is_empty(),
        "no archive should be taken when the run declines"
    );
    assert!(
        progress.dump().contains("No btrbk snapshot named"),
        "must say why it declined: {}",
        progress.dump()
    );
}
