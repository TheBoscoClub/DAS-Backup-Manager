//! The `btrdasd config derive-recovery` command line: its exit status, its
//! streams and what it leaves on disk.
//!
//! `Config::derive_recovery` and `validate` are tested on their own in
//! `config.rs`; nothing there reaches the handler in `main.rs` that joins them
//! — derive, then refuse an invalid result before anything is written. A
//! handler that wrote an invalid derived configuration (the `!` of its
//! `if !errors.is_empty()` deleted) passed every one of those tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use buttered_dasd::config::Config;

const BOOTED_B: &str = "7c7ae72d-09d6-4086-b249-1ac60f21b73b";

const A_IDENTITY: &str = "serial = \"S-A\"\nmount_uuid = \"60b05268-7f8f-47b5-a38a-752576a1172a\"";

fn target(label: &str, identity: &str, mount: &str, role: &str) -> String {
    format!(
        "[[target]]\nlabel = \"{label}\"\n{identity}\nmount = \"{mount}\"\nrole = \"{role}\"\n[target.retention]\ndaily = 7\n"
    )
}

/// One source and three targets; `a_identity` is the `serial`/`mount_uuid`
/// lines of target A, so a case can leave A without any way to find it.
fn host_toml(a_identity: &str) -> String {
    let b = format!("serial = \"S-B\"\nmount_uuid = \"{BOOTED_B}\"");
    let primary = "serial = \"S-P\"\nmount_uuid = \"b2dbe07d-40b9-422e-8ccf-ef4931c40457\"";
    format!(
        r#"
[general]
version = "0.4.0"
install_prefix = "/usr/local"
db_path = "/var/lib/das-backup/backup-index.db"

[init]
system = "systemd"

[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30

[email]
enabled = false

[gui]
enabled = false

[[source]]
label = "s"
volume = "/vol"
device = "/dev/sda"
[[source.subvolumes]]
name = "a"

{}{}{}"#,
        target("primary-22tb", primary, "/mnt/backup-22tb", "primary"),
        target(
            "system-recovery-A-2tb",
            a_identity,
            "/mnt/backup-system-recovery-A",
            "mirror"
        ),
        target(
            "system-recovery-B-2tb",
            &b,
            "/mnt/backup-system-recovery-B",
            "mirror"
        ),
    )
}

fn write_host(dir: &Path, a_identity: &str) -> PathBuf {
    let path = dir.join("host.toml");
    std::fs::write(&path, host_toml(a_identity)).expect("write host config");
    path
}

fn derive(from: &Path, uuid: &str, out: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_btrdasd"));
    cmd.args(["config", "derive-recovery", "--from"])
        .arg(from)
        .args(["--booted-uuid", uuid]);
    if let Some(out) = out {
        cmd.arg("--out").arg(out);
    }
    cmd.output().expect("spawn btrdasd")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("UTF-8 output")
}

fn check_derived(cfg: &Config) {
    assert!(cfg.general.recovery_drive);
    assert!(cfg.sources.is_empty());
    let labels: Vec<&str> = cfg.targets.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "system-recovery-B-2tb",
            "primary-22tb",
            "system-recovery-A-2tb"
        ]
    );
}

#[test]
fn valid_config_is_written_to_out_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let from = write_host(dir.path(), A_IDENTITY);
    let out = dir.path().join("derived.toml");
    let o = derive(&from, BOOTED_B, Some(&out));
    assert_eq!(o.status.code(), Some(0), "stderr: {}", text(&o.stderr));
    assert_eq!(text(&o.stdout), "");
    assert_eq!(text(&o.stderr), "");
    let cfg = Config::from_toml(&std::fs::read_to_string(&out).expect("--out file exists"))
        .expect("derived file parses");
    check_derived(&cfg);
}

#[test]
fn valid_config_goes_to_stdout_without_out() {
    let dir = tempfile::tempdir().unwrap();
    let from = write_host(dir.path(), A_IDENTITY);
    let o = derive(&from, BOOTED_B, None);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", text(&o.stderr));
    assert_eq!(text(&o.stderr), "");
    let cfg = Config::from_toml(&text(&o.stdout)).expect("stdout is the derived TOML");
    check_derived(&cfg);
}

#[test]
fn unknown_uuid_exits_one_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let from = write_host(dir.path(), A_IDENTITY);
    let out = dir.path().join("derived.toml");
    let o = derive(&from, "00000000-0000-0000-0000-000000000000", Some(&out));
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(text(&o.stdout), "");
    let err = text(&o.stderr);
    assert!(
        err.starts_with("derive-recovery: no target of filesystem"),
        "{err}"
    );
    assert!(!out.exists(), "--out must not be created");
}

#[test]
fn derived_config_that_fails_validation_exits_one_and_writes_nothing() {
    // Target A has no serial, serials or mount_uuid: derivation (which only
    // looks at the booted target) succeeds, validation refuses the result.
    let dir = tempfile::tempdir().unwrap();
    let from = write_host(dir.path(), "");
    let out = dir.path().join("derived.toml");
    let o = derive(&from, BOOTED_B, Some(&out));
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "stderr: {err}");
    assert_eq!(text(&o.stdout), "");
    assert!(err.starts_with("derive-recovery: "), "{err}");
    assert!(
        err.contains("needs at least one of: `serial`, `serials`, or `mount_uuid`"),
        "{err}"
    );
    assert!(!out.exists(), "--out must not be created");

    // Same refusal without --out: nothing reaches stdout.
    let o = derive(&from, BOOTED_B, None);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(text(&o.stdout), "");
}

#[test]
fn unwritable_out_exits_one_naming_the_failure() {
    // A regular file where the parent directory should be: the directory
    // cannot be created (a merely missing parent is created by `save`).
    let dir = tempfile::tempdir().unwrap();
    let from = write_host(dir.path(), A_IDENTITY);
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let out = blocker.join("derived.toml");
    let o = derive(&from, BOOTED_B, Some(&out));
    let err = text(&o.stderr);
    assert_eq!(o.status.code(), Some(1), "stderr: {err}");
    assert_eq!(text(&o.stdout), "");
    assert!(err.contains("cannot write"), "{err}");
    assert!(!out.exists());
}
