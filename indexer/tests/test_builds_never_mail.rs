//! No test build can hand a report to a real relay (bd DAS-Backup-Manager-1x76).
//!
//! On 2026-10-06 a local mutation run mailed four reports through the live
//! relay: a mutant flipped the email gate and the test it ran spawned the real
//! `mailx`. Every report send now goes through `report::Mailer`, and the
//! production mailer (`report::MAILX`) refuses, before it spawns anything, in
//! every build that has the `test-overrides` feature. The crate's
//! dev-dependency on itself turns it on, so every target `cargo test` builds
//! has it: this integration test, the library's own unit tests
//! (`report::tests::a_test_build_never_spawns_mailx`), the binaries' unit
//! tests and the binaries these tests run. Take the feature off that
//! dev-dependency and these tests fail. The shipped direction, a build without
//! the feature mailing, is `tests/test_report_mail.sh`.
//!
//! Each case stands a recording stub where `mailx` would be, shows that the
//! stub records a run of itself, and then that nothing ran it.

use buttered_dasd::report::{self, Mailer};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

/// Held while a stub is written and run, and while either test spawns. The
/// two tests run in parallel: a fork in one while the other still has its
/// stub open for writing hands the child that write descriptor, and the exec
/// of the stub then fails with ETXTBSY ("Text file busy"). Measured: 32 of
/// 300 runs failed that way at the default test threads, 0 of 300 with this
/// lock (bd DAS-Backup-Manager-1x76).
static SPAWN: Mutex<()> = Mutex::new(());

/// The spawn lock; a test that failed while holding it does not poison it for
/// the other.
fn spawn_lock() -> MutexGuard<'static, ()> {
    SPAWN.lock().unwrap_or_else(|e| e.into_inner())
}

/// A `mailx` stand-in at `<dir>/mailx` that appends its arguments to
/// `<dir>/ran` and exits 0. It calls no mailer and never its own name.
/// Returns the stub and the record it writes, the stub seen to write it.
fn recording_stub(dir: &Path) -> (PathBuf, PathBuf) {
    let _spawning = spawn_lock();
    let marker = dir.join("ran");
    let stub = dir.join("mailx");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/bash\nprintf '%s\\n' \"$@\" >>'{}'\ncat >/dev/null\nexit 0\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The stub drains its stdin, as mailx reads the body: an inherited one
    // (a terminal, an open pipe) would never end and the probe would hang.
    let probe = Command::new(&stub)
        .arg("probe")
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(probe.success(), "the stub runs");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "probe\n");
    std::fs::remove_file(&marker).unwrap();
    (stub, marker)
}

#[test]
fn the_library_in_an_integration_build_never_spawns_mailx() {
    let dir = tempfile::tempdir().unwrap();
    let (stub, marker) = recording_stub(dir.path());
    let program: &'static str = Box::leak(stub.to_string_lossy().into_owned().into_boxed_str());
    let message = report::MailMessage {
        subject: "[DAS Backup] h — SUCCESS — 2026-11-01 01:02".into(),
        from: "DAS Backup (h) <das@example.org>".into(),
        to: "ops@example.org".into(),
        mta: "smtp://127.0.0.1:1".into(),
        body: "x".into(),
    };
    let err = report::Mailx::new(program).send(&message).unwrap_err();
    assert_eq!(
        err,
        format!("{program} not run: a test build never mails (bd DAS-Backup-Manager-1x76)")
    );
    assert!(!marker.exists(), "nothing was spawned");
}

/// The `btrdasd` these tests run, with a stub `mailx` first on its `PATH`:
/// `doctor --email` with no sources reaches a report send on every path (the
/// locks cannot be taken unprivileged: the could-not-run report; as root, a
/// check of nothing: the not-ran report), and the binary refuses it.
#[test]
fn the_binary_these_tests_run_never_spawns_mailx() {
    let dir = tempfile::tempdir().unwrap();
    let (_stub, marker) = recording_stub(dir.path());
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"[general]
version = "0.7.23"
install_prefix = "/usr"
db_path = "{db}"
btrbk_conf = "{conf}"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[email]
enabled = true
to = "ops@example.org"
from = "das@example.org"
smtp_host = "127.0.0.1"
smtp_port = 1
[gui]
enabled = false
"#,
            db = dir.path().join("index.db").display(),
            conf = dir.path().join("btrbk.conf").display(),
        ),
    )
    .unwrap();
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = {
        let _spawning = spawn_lock();
        Command::new(env!("CARGO_BIN_EXE_btrdasd"))
            .args(["doctor", "--email", "--config"])
            .arg(&config)
            .env("PATH", path)
            .env_remove("DAS_REPORT_TO")
            .env_remove("DAS_REPORT_FROM")
            .output()
            .expect("spawn btrdasd")
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("report email: mailx not run: a test build never mails"),
        "the send was reached and refused: {stderr}"
    );
    assert!(!marker.exists(), "nothing was spawned: {stderr}");
}
