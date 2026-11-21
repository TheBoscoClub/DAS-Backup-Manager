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
        Self::new_sized(tag, 1)
    }

    /// Like [`Rig::new`] but with a caller-chosen image size in GiB, for tests
    /// that need more data than the 1 GiB default so a slowed scrub can be
    /// cancelled mid-pass and leave a genuine resumable position.
    fn new_sized(tag: &str, image_gib: u32) -> Self {
        let dir = PathBuf::from(format!(
            "/var/tmp/das-scrub-it-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create rig dir");
        let image = dir.join("disk.img");
        let mount_point = dir.join("mnt");
        std::fs::create_dir_all(&mount_point).expect("create mount dir");

        must(
            "truncate",
            &["-s", &format!("{image_gib}G"), image.to_str().unwrap()],
        );
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

    /// True when some other process still has the mount in use: an open file
    /// under it, or its working directory or root there. `btrfs scrub start`
    /// forks a daemon that holds the mount until it has written the status
    /// record and exited, and `btrfs scrub cancel` returns as soon as the
    /// kernel stops the scrub, which can be before that. A starved daemon is
    /// the CI flake `umount: target is busy` (bd DAS-Backup-Manager-uk3v.4).
    fn mount_in_use(&self) -> bool {
        let me = std::process::id().to_string();
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return true; // cannot look: assume busy, never a false "idle"
        };
        for entry in procs.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            if pid == me {
                continue;
            }
            let base = entry.path();
            let mut links = vec![base.join("cwd"), base.join("root")];
            if let Ok(fds) = std::fs::read_dir(base.join("fd")) {
                links.extend(fds.flatten().map(|f| f.path()));
            }
            if links
                .iter()
                .filter_map(|l| std::fs::read_link(l).ok())
                .any(|t| t.starts_with(&self.mount_point))
            {
                return true;
            }
        }
        false
    }

    /// Wait until nothing but this test uses the mount, so `umount` cannot
    /// meet a scrub daemon still on its way out. Returns whether it went idle
    /// within `timeout`.
    fn wait_mount_idle(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while self.mount_in_use() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        true
    }

    /// Unmount the rig, retrying while the mount is busy. This host also runs
    /// the CI runners and other jobs, and any of them may briefly look into a
    /// new mount (a scanner, a mount-table watcher); `umount` then says "target
    /// is busy" though nothing the test did holds it. Waits for idle first.
    fn umount_when_idle(&self) {
        let mnt = self.mount_point.to_str().unwrap();
        let mut last = String::new();
        for _ in 0..40 {
            let _ = self.wait_mount_idle(std::time::Duration::from_secs(60));
            let (ok, text) = run("umount", &[mnt]);
            if ok {
                return;
            }
            last = text;
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        panic!("umount {mnt} still failing after 10 s of retries: {last}");
    }

    /// Cancel a running scrub and wait for the scrub daemon to finish with the
    /// mount, which is also when it has written the status record. The cancel
    /// itself may fail (nothing running), so its result is ignored.
    fn cancel_scrub_and_settle(&self) {
        let mnt = self.mount_point.to_str().unwrap();
        let _ = run("btrfs", &["scrub", "cancel", mnt]);
        assert!(
            self.wait_mount_idle(std::time::Duration::from_secs(60)),
            "something still holds {mnt} 60 s after `btrfs scrub cancel`"
        );
    }

    fn status_record(&self) -> PathBuf {
        PathBuf::from(format!("/var/lib/btrfs/scrub.status.{}", self.uuid))
    }

    fn read_record(&self) -> String {
        std::fs::read_to_string(self.status_record()).unwrap_or_default()
    }

    /// Overwrite the saved record with the aborted shape — `canceled:0
    /// finished:0` — mirroring what a scrub killed by a USB drop or a reboot
    /// leaves behind, with a start time far in the past. Error counters are
    /// zero: this fixture represents a *clean* interruption (no errors found
    /// before the abort), so a resume that finds no further errors completes
    /// cleanly. Resume carries prior counters forward — unlike the old
    /// force-start, which reset them — so a nonzero here would surface in the
    /// completed record and is not what this fixture is testing.
    fn forge_aborted_record(&self, t_start: i64) {
        let record = format!(
            "scrub status:1\n{}:1|data_extents_scrubbed:4800|tree_extents_scrubbed:80|\
data_bytes_scrubbed:314572800|tree_bytes_scrubbed:1310720|read_errors:0|csum_errors:0|\
verify_errors:0|no_csum:0|csum_discards:0|super_errors:0|malloc_errors:0|\
uncorrectable_errors:0|corrected_errors:0|last_physical:660602880|t_start:{t_start}|\
t_resumed:0|duration:0|canceled:0|finished:0\n",
            self.uuid
        );
        std::fs::write(self.status_record(), record).expect("forge record");
    }

    /// Rewrite the *current* saved record's terminal flags to the aborted
    /// (crash) signature `canceled:0 finished:0`, keeping the real
    /// `last_physical` and counters intact. A clean `btrfs scrub cancel` writes
    /// `canceled:1 finished:1` (→ `Canceled`), but a reboot or unmount that
    /// kills a scrub mid-write leaves `canceled:0 finished:0` (→ `Aborted`).
    /// This produces that crash signature deterministically on top of a genuine
    /// partial position, which a cancel-then-race cannot do reliably.
    fn mark_record_aborted(&self) {
        let record = self.read_record();
        let aborted = record
            .replace("canceled:1", "canceled:0")
            .replace("finished:1", "finished:0");
        std::fs::write(self.status_record(), aborted).expect("rewrite record aborted");
    }

    /// Write `gib` GiB of incompressible data into the (temporarily mounted)
    /// filesystem, so a delayed scrub has enough to verify that it cannot
    /// finish before being cancelled.
    fn fill_incompressible_gib(&self, gib: u32) {
        self.with_temporary_mount(|mnt| {
            for i in 0..gib {
                must(
                    "dd",
                    &[
                        "if=/dev/urandom",
                        &format!("of={}/blob{i}.bin", mnt.display()),
                        "bs=1M",
                        "count=1024",
                        "status=none",
                    ],
                );
            }
            must("sync", &[]);
        });
    }

    /// `data_bytes_scrubbed` from the saved status record, summed is unnecessary
    /// on this single-device rig. Returns 0 if the field is absent.
    fn bytes_scrubbed(&self) -> u64 {
        self.record_field("data_bytes_scrubbed").unwrap_or(0)
    }

    /// `t_start` from the saved status record — the scrub's original start time,
    /// which `resume` preserves and a fresh `start` replaces.
    fn record_t_start(&self) -> u64 {
        self.record_field("t_start")
            .expect("record must carry t_start")
    }

    /// Parse a single `key:value` field from the pipe-delimited status record.
    fn record_field(&self, key: &str) -> Option<u64> {
        let record = self.read_record();
        record
            .split('|')
            .find_map(|f| f.trim().strip_prefix(&format!("{key}:")))
            .and_then(|v| v.trim().parse().ok())
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
        // Let a dying scrub daemon let go first, then retry: one `umount` that
        // meets it fails with "target is busy" and, with the mount still
        // there, so would `losetup -d` and the cleanup after it.
        let _ = self.wait_mount_idle(std::time::Duration::from_secs(60));
        for _ in 0..20 {
            if !run("mountpoint", &["-q", &mnt]).0 {
                break;
            }
            let _ = run("umount", &[&mnt]);
            std::thread::sleep(std::time::Duration::from_millis(250));
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
        esp_partuuid: None,
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

    // With nothing running, the engine must decide to RESUME (bd -292): an
    // aborted record carries a resumable position, so continuing it is
    // preferred over restarting from zero.
    rig.with_temporary_mount(|mnt| {
        assert_eq!(
            live_scrub_state(mnt.to_str().unwrap(), &rig.uuid),
            LiveScrubState::NotRunning,
            "no scrub should be running on the idle rig"
        );
        match scrub::decide_scrub_start_mode(&rig.uuid, mnt.to_str().unwrap()) {
            ScrubStartMode::Resume { reason } => println!("  decision: RESUME — {reason}"),
            ScrubStartMode::Normal => {
                panic!("aborted record with nothing running must resume")
            }
        }
    });

    let pass2 = with_env("DAS_SCRUB_STATE", state_file.to_str().unwrap(), || {
        scrub::run_scrub_pass(&config, &Echo).expect("pass must run")
    });
    let result2 = &pass2.results[0];
    // The invariant an aborted record must satisfy: it must NOT wedge scrubs.
    // Whether the forged position is accepted by `btrfs scrub resume` or falls
    // back to a fresh `start -f` (the forged `last_physical` may not match real
    // on-disk state), the pass must recover to a clean Finished either way —
    // that is what "an aborted record cannot block this filesystem" means.
    // Continuation from a *real* interrupted position is proven separately by
    // `interrupted_scrub_resumes_across_unmount`, where the position is genuine.
    assert!(
        result2.ok(),
        "an aborted record must not wedge the scrub — got {result2:?}"
    );
    assert_eq!(result2.outcome, Some(ScrubOutcome::Finished));
    let _ = first_success; // freshness is no longer pass 2's concern — see below

    // The falsifiable proof that the ENGINE (run_btrfs_scrub) actually issued a
    // RESUME, not a fresh start: resume preserves the interrupted record's
    // `t_start`, whereas `btrfs scrub start` stamps a new one. The forged
    // record's `t_start` is 1_700_000_000 (2023); a resumed completion inherits
    // it, a restarted one would carry ~now. This is the assertion that goes RED
    // if the Resume arm is reverted to a force-start — the runner dispatch is
    // otherwise invisible, because a forged near-complete position finishes
    // cleanly either way.
    assert_eq!(
        result2.started_epoch, 1_700_000_000,
        "engine must RESUME (preserving the forged t_start), not restart fresh — \
         got started_epoch {}",
        result2.started_epoch
    );

    // NOTE: this pass deliberately does NOT assert the success timestamp
    // advanced past pass 1. Resume preserves the interrupted record's
    // `t_start`, and `last_success_epoch` is derived from it, so a resumed
    // completion inherits the prior (2023) start time rather than stamping a
    // fresh one — that is the whole point of resume. Continuation from a *real*
    // interrupted position is proven by `interrupted_scrub_resumes_across_unmount`.
    println!("== both passes OK ==");
}

/// The load-bearing claim of bd DAS-Backup-Manager-292: a scrub interrupted
/// mid-pass RESUMES from its saved position on the next run rather than
/// restarting from zero — and the position survives an unmount/remount cycle,
/// which is the case that matters for the DAS targets (unmounted between runs
/// by design). This is the honest counter-test: the interrupted position is
/// genuine (a real scrub cancelled mid-flight), not a forged record.
///
/// Reproduces, in-tree, the manual experiment run on 2026-09-09 that observed
/// `data_bytes_scrubbed` advancing 997 MB → 9.58 GB across the cycle.
#[test]
#[ignore = "requires root; creates loop devices and issues real scrubs"]
fn interrupted_scrub_resumes_across_unmount() {
    assert!(is_root(), "must run as root");
    // 3 GiB image, 2 GiB of incompressible data. The pass is made slow by a
    // dm-delay device, NOT by `scrub_speed_max`: on kernel 7.3.5 that knob
    // reads back the value written yet does not throttle (2.6 GB scrubbed in
    // under 1 s), so the scrub finished before the cancel and the final
    // assertion passed or failed by luck (bd DAS-Backup-Manager-uk3v.9).
    let mut rig = Rig::new_sized("resume", 3);
    rig.fill_incompressible_gib(2);
    let slow_dev = rig.attach_slow_device(RESUME_READ_DELAY_MS);
    let mnt = rig.mount_point.to_string_lossy().to_string();
    must("mount", &[&slow_dev, &mnt]);

    // Start a real scrub, let it make progress, then stop it to freeze a real
    // partial position. A clean `btrfs scrub cancel` records `canceled:1
    // finished:1`; `mark_record_aborted` then rewrites the terminal flags to
    // the crash signature `canceled:0 finished:0` — deterministically
    // producing an *aborted* record on top of a genuine mid-pass position,
    // which is what a reboot/unmount kill leaves behind (and what a cancel race
    // produces only unreliably).
    // NOT `must`/`Command::output()`: `btrfs scrub start` (without -B) forks a
    // daemon that inherits stdout/stderr, so reading them to EOF blocks until
    // the whole scrub finishes — which would defeat the delay and leave
    // nothing to cancel mid-flight. Null the descriptors, wait only for the
    // direct child. (Same trap and remedy as `live_scrub_is_never_force_restarted`.)
    let start = Command::new("btrfs")
        .args(["scrub", "start", &mnt])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn btrfs scrub start");
    assert!(start.success(), "background scrub failed to start");
    // Sleep well past the metadata/system-chunk phase (data_bytes stays 0 for
    // the first few seconds), then require the scrub to still be running: the
    // cancel must land mid-data or the test proves nothing about a resume.
    std::thread::sleep(std::time::Duration::from_secs(RESUME_RUN_SECS));
    let live = live_scrub_state(&mnt, &rig.uuid);
    // Settle, not just cancel: the daemon writes the record and exits after
    // the kernel stops the scrub, and the reads and the unmount below need both.
    rig.cancel_scrub_and_settle();
    assert_eq!(
        live,
        LiveScrubState::Running,
        "the scrub was not running {RESUME_RUN_SECS} s in (state {live:?}), so the cancel \
         would not interrupt it and a resume would prove nothing; the scrub finished or \
         failed before the cancel (slow device {slow_dev}, {RESUME_READ_DELAY_MS} ms/read); \
         record:\n{}",
        rig.read_record()
    );
    assert!(
        rig.read_record().contains("canceled:1"),
        "the cancel did not catch a running scrub (record lacks canceled:1):\n{}",
        rig.read_record()
    );
    let bytes_at_cancel = rig.bytes_scrubbed();
    assert!(
        bytes_at_cancel > 0,
        "scrub must have made real progress before cancel (got {bytes_at_cancel})"
    );
    let t_start_at_cancel = rig.record_t_start();
    println!("  stopped at {bytes_at_cancel} bytes, t_start={t_start_at_cancel}");

    // The DAS case: the filesystem is unmounted between runs. Prove the saved
    // position (host-side, keyed by FS UUID) survives an unmount/remount, and
    // stamp the crash signature onto it while it is unmounted.
    rig.umount_when_idle();
    assert!(
        rig.status_record().exists(),
        "scrub.status.<uuid> must persist on the host root while unmounted"
    );
    rig.mark_record_aborted();
    must("mount", &[&slow_dev, &mnt]);
    assert!(
        rig.read_record().contains("finished:0") && rig.read_record().contains("canceled:0"),
        "the interrupted record must read as aborted after remount"
    );

    // The engine must now decide to RESUME, and the pass must continue from the
    // cancelled position — not restart. Two independent proofs of continuation:
    //   1. t_start is PRESERVED (a fresh start would stamp a new one)
    //   2. the finished pass scrubbed MORE than was done at cancel, i.e. it
    //      picked up the remainder rather than re-doing the whole filesystem
    match scrub::decide_scrub_start_mode(&rig.uuid, &mnt) {
        ScrubStartMode::Resume { .. } => println!("  decision: RESUME (correct)"),
        ScrubStartMode::Normal => panic!("a genuine interrupted scrub must resume"),
    }
    must("btrfs", &["scrub", "resume", "-B", &mnt]);

    let rec = rig.read_record();
    assert!(
        rec.contains("finished:1"),
        "resumed scrub must run to completion, got:\n{rec}"
    );
    let t_start_after = rig.record_t_start();
    assert_eq!(
        t_start_after, t_start_at_cancel,
        "resume must PRESERVE the original t_start ({t_start_at_cancel}); a fresh \
         start would have replaced it (got {t_start_after})"
    );
    let bytes_final = rig.bytes_scrubbed();
    assert!(
        bytes_final > bytes_at_cancel,
        "resume must continue past the interrupted position: {bytes_at_cancel} \
         (at cancel) -> {bytes_final} (finished) — this is the 997MB->9.58GB \
         advance observed in the manual experiment"
    );
    println!(
        "  resumed to completion; t_start preserved at {t_start_after}; \
         bytes {bytes_at_cancel} -> {bytes_final}"
    );
    let _ = run("btrfs", &["scrub", "cancel", &mnt]); // tidy; already finished, so ignore
    println!("== resume-across-unmount verified ==");
}

/// Per-read delay of the dm-delay device the resume test scrubs through.
const RESUME_READ_DELAY_MS: u32 = 100;
/// Seconds the resume test lets the scrub run before cancelling it.
const RESUME_RUN_SECS: u64 = 8;

/// A scrub started by somebody else must never be resumed or restarted over:
/// hours of work would be thrown away. The engine must return `Normal` and let
/// the plain start fail loudly instead.
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
        state = live_scrub_state(&mnt, &rig.uuid);
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
    // would otherwise trigger a resume. Liveness must still win.
    rig.forge_aborted_record(1_700_000_000);
    match scrub::decide_scrub_start_mode(&rig.uuid, &mnt) {
        ScrubStartMode::Normal => println!("  decision: NORMAL (correct — a scrub is running)"),
        ScrubStartMode::Resume { reason } => {
            panic!("must not resume over a running scrub, but chose to: {reason}")
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

// A live loopback test reproducing a genuine `Command::spawn()` failure for
// `btrfs scrub start` (the `bd DAS-Backup-Manager-18p` review scenario) was
// attempted here and removed: shadowing "btrfs" earlier in `PATH` with a
// non-executable stub, a directory, or a script with a broken `#!`
// interpreter all failed to force a spawn error — verified empirically
// (`std::process::Command`'s PATH search, like glibc's `execvp`, silently
// continues past `EACCES`/bad-interpreter entries to the next `PATH`
// component and finds the real `/usr/bin/btrfs` regardless). Reliably
// forcing this at the OS level would require either touching the real
// system `btrfs` binary (unsafe on a host with other live scrubs and
// mounted filesystems) or breaking `PATH` so thoroughly that `mount`/
// `findmnt`/`umount` fail too, which changes the scenario to "the mount
// step failed", not "resolution and mounting succeeded but only the scrub
// spawn failed". `exit_code_for_pass_spawn_failure_on_all_targets_is_nonzero`
// in `indexer/src/main.rs` is the unit-test proof instead, per the
// reviewer's own explicit fallback for exactly this case.
