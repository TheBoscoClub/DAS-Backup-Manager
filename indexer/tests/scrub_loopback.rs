//! End-to-end scrub-engine tests against a real BTRFS filesystem on a loop
//! device. These are `#[ignore]`d and root-gated: they create loop devices,
//! device-mapper targets, and real mounts, and they run genuine
//! `btrfs scrub` passes.
//!
//! Run with:
//!
//! ```text
//! sudo -E cargo test --target-dir ../build/cargo-target \
//!     --test scrub_loopback -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `--test-threads=1` matters: both tests take the real
//! `/run/das-scrub.lock` and `/run/das-maintenance.lock`, and both manipulate
//! process-wide environment variables.

use std::path::{Path, PathBuf};
use std::process::Command;

use buttered_dasd::config::{Config, Retention, Target, TargetRole};
use buttered_dasd::progress::{LogLevel, ProgressCallback};
use buttered_dasd::scrub::{
    self, LiveScrubState, PassStatus, ScrubOutcome, ScrubStartMode, live_scrub_state,
};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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

/// Loop-backed BTRFS filesystem that tears itself down on drop — including on
/// panic, so a failed assertion never leaves a loop device or a stray record
/// in `/var/lib/btrfs` behind.
struct Rig {
    dir: PathBuf,
    image: PathBuf,
    loop_dev: String,
    uuid: String,
    mount_point: PathBuf,
    dm_name: Option<String>,
}

impl Rig {
    fn new(tag: &str) -> Self {
        let dir = PathBuf::from(format!(
            "/var/tmp/das-scrub-it-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create rig dir");
        let image = dir.join("disk.img");
        let mount_point = dir.join("mnt");
        std::fs::create_dir_all(&mount_point).expect("create mount dir");

        must("truncate", &["-s", "1G", image.to_str().unwrap()]);
        let loop_dev = must("losetup", &["--find", "--show", image.to_str().unwrap()])
            .trim()
            .to_string();
        must("mkfs.btrfs", &["-q", "-L", "das-scrub-it", &loop_dev]);
        let uuid = must("blkid", &["-s", "UUID", "-o", "value", &loop_dev])
            .trim()
            .to_string();
        assert!(!uuid.is_empty(), "mkfs produced no UUID");

        let rig = Rig {
            dir,
            image,
            loop_dev,
            uuid,
            mount_point,
            dm_name: None,
        };
        // Seed with real data so the scrub has something to verify.
        rig.with_temporary_mount(|mnt| {
            must(
                "dd",
                &[
                    "if=/dev/zero",
                    &format!("of={}/data.bin", mnt.display()),
                    "bs=1M",
                    "count=300",
                    "status=none",
                ],
            );
            must("sync", &[]);
        });
        rig
    }

    /// Mount, run `f`, unmount — used for setup only; the engine does its own
    /// mounting.
    fn with_temporary_mount(&self, f: impl FnOnce(&Path)) {
        must(
            "mount",
            &[
                "-t",
                "btrfs",
                &format!("UUID={}", self.uuid),
                self.mount_point.to_str().unwrap(),
            ],
        );
        f(&self.mount_point);
        must("umount", &[self.mount_point.to_str().unwrap()]);
    }

    fn status_record(&self) -> PathBuf {
        PathBuf::from(format!("/var/lib/btrfs/scrub.status.{}", self.uuid))
    }

    fn read_record(&self) -> String {
        std::fs::read_to_string(self.status_record()).unwrap_or_default()
    }

    /// Overwrite the saved record with the aborted shape — `canceled:0
    /// finished:0` — mirroring what a scrub killed by a USB drop leaves
    /// behind, with a start time far in the past.
    fn forge_aborted_record(&self, t_start: i64) {
        let record = format!(
            "scrub status:1\n{}:1|data_extents_scrubbed:4800|tree_extents_scrubbed:80|\
data_bytes_scrubbed:314572800|tree_bytes_scrubbed:1310720|read_errors:0|csum_errors:0|\
verify_errors:0|no_csum:0|csum_discards:0|super_errors:3|malloc_errors:0|\
uncorrectable_errors:0|corrected_errors:0|last_physical:660602880|t_start:{t_start}|\
t_resumed:0|duration:0|canceled:0|finished:0\n",
            self.uuid
        );
        std::fs::write(self.status_record(), record).expect("forge record");
    }

    /// A device-mapper shim that delays every read, so a scrub started against
    /// it stays running long enough to be observed.
    fn attach_slow_device(&mut self, delay_ms: u32) -> String {
        let name = format!("das-scrub-it-slow-{}", std::process::id());
        let sectors = must("blockdev", &["--getsz", &self.loop_dev])
            .trim()
            .to_string();
        // btrfs must forget the raw loop device, or mounts and scrubs resolve
        // to it instead of the delayed mapping.
        let _ = run("btrfs", &["device", "scan", "--forget", &self.loop_dev]);
        must(
            "dmsetup",
            &[
                "create",
                &name,
                "--table",
                &format!("0 {sectors} delay {} 0 {delay_ms}", self.loop_dev),
            ],
        );
        self.dm_name = Some(name.clone());
        format!("/dev/mapper/{name}")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let mnt = self.mount_point.to_str().unwrap().to_string();
        // Cancel anything still scrubbing, then unmount (possibly twice —
        // a test may have mounted it itself).
        let _ = run("btrfs", &["scrub", "cancel", &mnt]);
        for _ in 0..3 {
            if !run("mountpoint", &["-q", &mnt]).0 {
                break;
            }
            let _ = run("umount", &[&mnt]);
        }
        if let Some(name) = &self.dm_name {
            let _ = run("dmsetup", &["remove", name]);
        }
        let _ = run("btrfs", &["device", "scan", "--forget", &self.loop_dev]);
        let _ = run("losetup", &["-d", &self.loop_dev]);
        // Do not leave a record for a UUID that no longer exists.
        let _ = std::fs::remove_file(self.status_record());
        let _ = std::fs::remove_file(&self.image);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Config pointing the engine at the rig and nothing else.
fn rig_config(rig: &Rig) -> Config {
    let mut config = Config::default();
    config.das.mount_opts = "noatime".into();
    config.email.enabled = false; // never send mail from a test
    config.targets.push(Target {
        label: "loopback-rig".into(),
        serial: String::new(),
        serials: Vec::new(),
        mount_uuid: Some(rig.uuid.clone()),
        mount: rig.mount_point.to_string_lossy().to_string(),
        role: TargetRole::Primary,
        retention: Retention::default(),
        display_name: "loopback rig".into(),
    });
    config.scrub.targets = vec!["loopback-rig".into()];
    config
}

/// Prints everything the engine reports, so `--nocapture` yields a transcript.
struct Echo;
impl ProgressCallback for Echo {
    fn on_stage(&self, stage: &str, total: u64) {
        println!("  [stage] {stage} ({total} steps)");
    }
    fn on_progress(&self, current: u64, total: u64, message: &str) {
        println!("  [{current}/{total}] {message}");
    }
    fn on_throughput(&self, _: u64) {}
    fn on_log(&self, level: LogLevel, message: &str) {
        println!("  [{level:?}] {message}");
    }
    fn on_complete(&self, success: bool, summary: &str) {
        println!("  [done ok={success}] {summary}");
    }
}

/// Set an env var for the duration of `f` (tests run single-threaded).
fn with_env<R>(key: &str, value: &str, f: impl FnOnce() -> R) -> R {
    let previous = std::env::var(key).ok();
    // SAFETY: this test binary is run with --test-threads=1.
    unsafe { std::env::set_var(key, value) };
    let out = f();
    unsafe {
        match previous {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full pass over a real filesystem: mount by UUID, verify, scrub, parse,
/// persist, unmount — then the same again with an aborted record in the way,
/// which must be cleared with `-f` and produce a genuinely fresh scrub.
#[test]
#[ignore = "requires root; creates loop devices and runs real scrubs"]
fn loopback_full_pass_then_forced_recovery_from_aborted_record() {
    assert!(is_root(), "must run as root");
    let rig = Rig::new("pass");
    let config = rig_config(&rig);
    let state_file = rig.dir.join("scrub-state.json");

    println!("== rig: uuid={} loop={} ==", rig.uuid, rig.loop_dev);
    assert!(
        !run("mountpoint", &["-q", rig.mount_point.to_str().unwrap()]).0,
        "rig must start unmounted — the engine owns the mount"
    );

    // ---- pass 1: the ordinary path -------------------------------------
    println!("== pass 1: ordinary scrub ==");
    let pass = with_env("DAS_SCRUB_STATE", state_file.to_str().unwrap(), || {
        scrub::run_scrub_pass(&config, &Echo).expect("pass must run")
    });

    assert_eq!(pass.status, PassStatus::Completed);
    assert_eq!(pass.results.len(), 1);
    let result = &pass.results[0];
    assert!(result.ok(), "expected a clean scrub, got {result:?}");
    assert_eq!(result.outcome, Some(ScrubOutcome::Finished));
    assert_eq!(result.fsuuid, rig.uuid);
    assert!(
        result.bytes_scrubbed > 300 * 1024 * 1024,
        "expected the seeded 300 MiB to be scrubbed, got {}",
        result.bytes_scrubbed
    );
    assert!(result.mounted_by_engine, "the engine must own the mount");
    assert!(pass.success());

    // The engine unmounted what it mounted.
    assert!(
        !run("mountpoint", &["-q", rig.mount_point.to_str().unwrap()]).0,
        "engine must unmount the target when done"
    );

    // State landed, readable without the filesystem mounted.
    let state = scrub::load_state_from(&state_file).expect("state must parse");
    let fs_state = state
        .filesystems
        .get(&rig.uuid)
        .expect("state keyed by FS UUID");
    assert_eq!(fs_state.target_label, "loopback-rig");
    assert!(fs_state.last_attempt.ok);
    assert_eq!(fs_state.last_attempt.outcome, "finished");
    let first_success = fs_state.last_success_epoch.expect("success recorded");
    let mode = std::fs::metadata(&state_file)
        .unwrap()
        .permissions()
        .readonly();
    assert!(!mode);
    println!("  state: last_success_epoch={first_success}");

    // ---- pass 2: an aborted record is in the way ------------------------
    println!("== pass 2: aborted record present ==");
    rig.forge_aborted_record(1_700_000_000); // 2023-11-14
    assert!(rig.read_record().contains("finished:0"));

    // With nothing running, the engine must decide to force.
    rig.with_temporary_mount(|mnt| {
        assert_eq!(
            live_scrub_state(mnt.to_str().unwrap()),
            LiveScrubState::NotRunning,
            "no scrub should be running on the idle rig"
        );
        match scrub::decide_scrub_start_mode(&rig.uuid, mnt.to_str().unwrap()) {
            ScrubStartMode::Forced { reason } => println!("  decision: FORCED — {reason}"),
            ScrubStartMode::Normal => panic!("aborted record with nothing running must force"),
        }
    });

    let pass2 = with_env("DAS_SCRUB_STATE", state_file.to_str().unwrap(), || {
        scrub::run_scrub_pass(&config, &Echo).expect("pass must run")
    });
    let result2 = &pass2.results[0];
    assert!(
        result2.ok(),
        "forced scrub must recover cleanly, got {result2:?}"
    );
    assert_eq!(result2.outcome, Some(ScrubOutcome::Finished));
    // The decisive assertion: a genuinely fresh scrub, not the forged history.
    assert!(
        result2.started_epoch > 1_700_000_000,
        "scrub must be fresh, not the forged 2023 record (got {})",
        result2.started_epoch
    );
    assert!(
        result2.bytes_scrubbed > 300 * 1024 * 1024,
        "forced scrub must re-verify the whole filesystem, got {}",
        result2.bytes_scrubbed
    );
    let state2 = scrub::load_state_from(&state_file).expect("state must parse");
    assert!(
        state2.filesystems[&rig.uuid].last_success_epoch.unwrap() >= first_success,
        "success timestamp must advance"
    );
    println!("== both passes OK ==");
}

/// A scrub started by somebody else must never be force-restarted: hours of
/// work would be thrown away. The engine must return `Normal` and let the
/// plain start fail loudly instead.
#[test]
#[ignore = "requires root; creates loop and device-mapper devices"]
fn live_scrub_is_never_force_restarted() {
    assert!(is_root(), "must run as root");
    let mut rig = Rig::new("live");

    // Delay every read so the scrub stays running while we inspect it.
    let slow_dev = rig.attach_slow_device(400);
    let mnt = rig.mount_point.to_string_lossy().to_string();
    must("mount", &[&slow_dev, &mnt]);
    println!("== rig mounted via {slow_dev} (400ms/read) ==");

    // Start a real background scrub.
    //
    // Deliberately NOT `Command::output()`: `btrfs scrub start` forks a daemon
    // that inherits stdout/stderr, so reading them to EOF blocks until the
    // whole scrub finishes — which would defeat the point of this test. Null
    // the descriptors and wait only for the direct child.
    let status = Command::new("btrfs")
        .args(["scrub", "start", &mnt])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn btrfs scrub start");
    assert!(status.success(), "background scrub failed to start");
    println!("  background scrub started");

    // Wait for the kernel to report it, so the assertion is not racing.
    let mut state = LiveScrubState::Unknown;
    for _ in 0..40 {
        state = live_scrub_state(&mnt);
        if state == LiveScrubState::Running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    assert_eq!(
        state,
        LiveScrubState::Running,
        "expected a live scrub on the delayed device"
    );
    println!("  live_scrub_state = Running");

    // Now put an aborted record in place as well — the one condition that
    // would otherwise trigger `-f`. Liveness must still win.
    rig.forge_aborted_record(1_700_000_000);
    match scrub::decide_scrub_start_mode(&rig.uuid, &mnt) {
        ScrubStartMode::Normal => println!("  decision: NORMAL (correct — a scrub is running)"),
        ScrubStartMode::Forced { reason } => {
            panic!("must not force over a running scrub, but chose to: {reason}")
        }
    }

    // And confirm why that matters: btrfs itself refuses a plain start here.
    let (ok, text) = run("btrfs", &["scrub", "start", "-B", &mnt]);
    assert!(!ok, "btrfs should refuse to start a second scrub");
    assert!(
        text.contains("already running"),
        "expected 'already running', got: {text}"
    );
    println!("  plain start correctly refused: {}", text.trim());

    must("btrfs", &["scrub", "cancel", &mnt]);
    println!("== live-scrub protection verified ==");
}
