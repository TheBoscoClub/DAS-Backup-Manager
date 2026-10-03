//! `btrdasd recovery-os`: the exit codes the backup run acts on, and the
//! state file `btrdasd health` reads (bd DAS-Backup-Manager-xd3). Every OS
//! root here is a fixture directory; nothing is mounted.

use std::path::Path;
use std::process::{Command, Output};

use buttered_dasd::caldate::{date_of, day_number, today};

fn btrdasd(args: &[&str], state: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_btrdasd"));
    cmd.args(args);
    if let Some(s) = state {
        cmd.env("DAS_RECOVERY_OS_STATE", s);
    }
    cmd.output().expect("spawn btrdasd")
}

fn host_kernel() -> String {
    let out = Command::new("uname").arg("-r").output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// A root upgraded `age` days ago, running the host's own kernel series.
fn os_root(root: &Path, age: i64) {
    let date = date_of(day_number(&today()).unwrap() - age);
    write(root, "etc/os-release", "PRETTY_NAME=\"Fixture OS\"\n");
    std::fs::create_dir_all(root.join("usr/lib/modules").join(host_kernel())).unwrap();
    write(
        root,
        "var/log/pacman.log",
        &format!(
            "[{date}T03:00:00+0000] [PACMAN] starting full system upgrade\n\
             [{date}T03:05:00+0000] [ALPM] transaction completed\n"
        ),
    );
    write(
        root,
        "var/lib/pacman/local/btrbk-0.32.6-1/desc",
        "%NAME%\nbtrbk\n\n%VERSION%\n0.32.6-1\n",
    );
}

fn config(dir: &Path, mirror_mount: &Path) -> std::path::PathBuf {
    let text = format!(
        r#"[general]
version = "0.7.22"
install_prefix = "/usr"
db_path = "{db}"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[recovery_os]
max_age_days = 30
[[source]]
label = "s"
volume = "/vol"
device = "UUID=abc"
[[source.subvolumes]]
name = "@"
[[target]]
label = "primary"
serial = "P"
mount = "/nonexistent/primary"
role = "primary"
[target.retention]
daily = 7
[[target]]
label = "recovery-A"
serial = "A"
mount = "{mirror}"
role = "mirror"
[target.retention]
daily = 7
[email]
enabled = false
[gui]
enabled = false
"#,
        db = dir.join("index.db").display(),
        mirror = mirror_mount.display(),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn inspect_exit_codes_follow_the_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &dir.path().join("unmounted"));
    let cfg = cfg.to_str().unwrap();
    let root = dir.path().join("os");
    os_root(&root, 1);
    let r = root.to_str().unwrap();

    let out = btrdasd(
        &[
            "recovery-os",
            "inspect",
            "--root",
            r,
            "--label",
            "A",
            "--config",
            cfg,
        ],
        None,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}{}",
        text(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text(&out).starts_with("RECOVERY OS\n  A  ("),
        "{}",
        text(&out)
    );
    assert!(
        text(&out).contains("    Result              current\n"),
        "{}",
        text(&out)
    );

    // Past the configured 30 days (not the default 60).
    os_root(&root, 31);
    let out = btrdasd(
        &[
            "recovery-os",
            "inspect",
            "--root",
            r,
            "--label",
            "A",
            "--config",
            cfg,
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("last full upgrade 31 days ago (limit 30)"),
        "{}",
        text(&out)
    );

    // No upgrade record at all: stale, and no age is made up.
    std::fs::remove_file(root.join("var/log/pacman.log")).unwrap();
    let out = btrdasd(
        &[
            "recovery-os",
            "inspect",
            "--root",
            r,
            "--label",
            "A",
            "--config",
            cfg,
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("    Last full upgrade   unknown\n"),
        "{}",
        text(&out)
    );
    assert!(
        text(&out).contains("STALE — last upgrade unknown"),
        "{}",
        text(&out)
    );

    let gone = dir.path().join("no-such-root");
    let out = btrdasd(
        &[
            "recovery-os",
            "inspect",
            "--root",
            gone.to_str().unwrap(),
            "--label",
            "A",
            "--config",
            cfg,
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).contains("UNREADABLE"), "{}", text(&out));

    let out = btrdasd(
        &[
            "recovery-os",
            "inspect",
            "--root",
            r,
            "--label",
            "A",
            "--config",
            "/nonexistent/c.toml",
        ],
        None,
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "an unreadable config cannot be checked against"
    );
}

#[test]
fn inspect_json_is_one_drive_object() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &dir.path().join("unmounted"));
    let root = dir.path().join("os");
    os_root(&root, 2);
    let out = btrdasd(
        &[
            "--json",
            "recovery-os",
            "inspect",
            "--root",
            root.to_str().unwrap(),
            "--label",
            "A",
            "--config",
            cfg.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(0));
    let j: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(j["label"], "A");
    assert_eq!(j["status"], "current");
    assert_eq!(j["assessment"]["age_days"], 2);
    assert_eq!(j["os"]["packages"]["btrbk"], "0.32.6-1");
}

#[test]
fn status_reports_an_unmounted_mirror_and_keeps_the_state_file() {
    let dir = tempfile::tempdir().unwrap();
    // A plain directory is not a mountpoint, so the drive is "not mounted".
    let mnt = dir.path().join("mnt");
    os_root(&mnt.join("@"), 1);
    let cfg = config(dir.path(), &mnt);
    let state = dir.path().join("recovery-os.json");
    let earlier = r#"{"schema_version":2,"drives":{"recovery-A":{"checked_epoch":1790000000,"os":{"os_name":"Old","last_full_upgrade_applied":"2026-01-01","last_full_upgrade_attempted":"2026-01-01","last_attempt_completed":true,"installed":"2025-12-01","log_read":true,"modules_read":true,"kernels":["6.1.0"],"packages":{},"packages_read":true,"problems":[]},"error":null}}}"#;
    std::fs::write(&state, earlier).unwrap();
    let out = btrdasd(
        &[
            "recovery-os",
            "status",
            "--config",
            cfg.to_str().unwrap(),
            "--state-file",
            state.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(text(&out), "RECOVERY OS\n  recovery-A  not mounted\n");
    let kept: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(
        kept["drives"]["recovery-A"]["checked_epoch"],
        1790000000_i64
    );

    // A state file that cannot be written fails the step (exit 2).
    let out = btrdasd(
        &[
            "recovery-os",
            "status",
            "--config",
            cfg.to_str().unwrap(),
            "--state-file",
            dir.path().join("no-dir/x.json").to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no-dir"));
    // A corrupt record is left alone, loudly, with the way out on one line.
    std::fs::write(&state, "{not json").unwrap();
    let out = btrdasd(
        &[
            "recovery-os",
            "status",
            "--config",
            cfg.to_str().unwrap(),
            "--state-file",
            state.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    let first = err.lines().next().unwrap_or("");
    assert!(
        first.starts_with("Error: could not record the result: ")
            && first.ends_with(&format!(
                "remove it to start over: rm -- '{}'",
                state.display()
            )),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&state).unwrap(), "{not json");

    // A record of the previous schema is refused the same way, naming its
    // version, and left alone; `health` says so too.
    let v1 = r#"{"schema_version":1,"drives":{}}"#;
    std::fs::write(&state, v1).unwrap();
    let out = btrdasd(
        &[
            "recovery-os",
            "status",
            "--config",
            cfg.to_str().unwrap(),
            "--state-file",
            state.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    let want = format!(
        "Error: could not record the result: {p}: record schema version 1, this btrdasd \
         reads 2 — left as it is; remove it to start over: rm -- '{p}'",
        p = state.display()
    );
    assert_eq!(err.lines().next(), Some(want.as_str()), "{err}");
    assert_eq!(std::fs::read_to_string(&state).unwrap(), v1);
    let out = btrdasd(&["health", "--config", cfg.to_str().unwrap()], Some(&state));
    let t = text(&out);
    assert!(
        t.contains(&format!(
            "recovery-A: not mounted; stored record unreadable: {}: record schema version 1",
            state.display()
        )),
        "{t}"
    );
}

/// A root installed `age` days ago and never upgraded since, the way the
/// production recovery OSes are: the log opens with the live ISO's
/// `pacman -b` install.
fn never_upgraded_root(root: &Path, age: i64) -> String {
    let date = date_of(day_number(&today()).unwrap() - age);
    write(root, "etc/os-release", "PRETTY_NAME=\"Fixture OS\"\n");
    std::fs::create_dir_all(root.join("usr/lib/modules").join(host_kernel())).unwrap();
    write(
        root,
        "var/log/pacman.log",
        &format!(
            "[{date}T23:23:05-0500] [PACMAN] Running 'pacman -b /mnt/var/lib/pacman -r /mnt -S base'\n\
             [{date}T23:31:00-0500] [ALPM] transaction completed\n"
        ),
    );
    date
}

#[test]
fn inspect_ages_a_never_upgraded_install_from_its_install_date() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &dir.path().join("unmounted"));
    let root = dir.path().join("os");
    let inspect = |json: bool| {
        let mut args = vec![];
        if json {
            args.push("--json");
        }
        args.extend([
            "recovery-os",
            "inspect",
            "--root",
            root.to_str().unwrap(),
            "--label",
            "A",
            "--config",
            cfg.to_str().unwrap(),
        ]);
        btrdasd(&args, None)
    };
    // Past the configured 30 days.
    let date = never_upgraded_root(&root, 40);
    let out = inspect(false);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let t = text(&out);
    for line in [
        format!("    Installed           {date}\n"),
        "    Last full upgrade   none recorded\n".to_string(),
        "    Age                 40 days since install\n".to_string(),
        format!("STALE — never upgraded since install on {date} (40 days)"),
    ] {
        assert!(t.contains(&line), "missing {line:?} in\n{t}");
    }
    let j: serde_json::Value = serde_json::from_slice(&inspect(true).stdout).unwrap();
    assert_eq!(j["os"]["installed"], date.as_str());
    assert_eq!(j["assessment"]["age_days"], 40);
    assert_eq!(j["assessment"]["age_basis"], "install");
    // Within the limit it is current.
    never_upgraded_root(&root, 10);
    let out = inspect(false);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out).contains("    Age                 10 days since install\n"),
        "{}",
        text(&out)
    );
}

#[test]
fn health_shows_the_stored_recovery_os_record_with_its_time() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), &dir.path().join("unmounted"));
    let state = dir.path().join("recovery-os.json");
    // Checked 2026-10-03 03:20 UTC, upgraded long before: stale.
    let checked = day_number("2026-10-03").unwrap() * 86_400 + 3 * 3600 + 20 * 60;
    let record = format!(
        r#"{{"schema_version":2,"drives":{{"recovery-A":{{"checked_epoch":{checked},"os":{{"os_name":"Old","last_full_upgrade_applied":"2026-03-14","last_full_upgrade_attempted":"2026-03-14","last_attempt_completed":true,"installed":"2026-02-01","log_read":true,"modules_read":true,"kernels":["6.1.0"],"packages":{{}},"packages_read":true,"problems":[]}},"error":null}}}}}}"#
    );
    std::fs::write(&state, record).unwrap();
    let out = btrdasd(&["health", "--config", cfg.to_str().unwrap()], Some(&state));
    let t = text(&out);
    assert!(
        t.contains(
            "  recovery-A (as of 2026-10-03 03:20 UTC): installed 2026-02-01, \
             last full upgrade 2026-03-14, age "
        ),
        "{t}"
    );
    assert!(
        t.contains("  - Recovery OS on 'recovery-A' is STALE: "),
        "{t}"
    );

    let out = btrdasd(
        &["--json", "health", "--config", cfg.to_str().unwrap()],
        Some(&state),
    );
    let j: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        j["recovery_os"][0].as_str().unwrap().starts_with(
            "recovery-A (as of 2026-10-03 03:20 UTC): installed 2026-02-01, \
                 last full upgrade 2026-03-14, age "
        ),
        "{j}"
    );
}
