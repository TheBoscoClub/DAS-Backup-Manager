//! `btrdasd subvol` commands that change the config must leave `config.toml`
//! and `btrbk.conf` agreeing with each other. Before this, `subvol add` wrote
//! the config and nothing else, so the subvolume was "added" and still not
//! backed up until `setup --upgrade` was also run.

use std::path::Path;
use std::process::Command;

/// `second_source` adds a source `s2` on the same volume and snapshot
/// directory as `s`: the layout where two sources can resolve to the same
/// snapshot name, which is the only way a collision survives the per-source
/// name resolver.
fn write_config_with(dir: &Path, second_source: bool) -> std::path::PathBuf {
    let conf = dir.join("btrbk.conf");
    let s2 = if second_source {
        "[[source]]\nlabel = \"s2\"\nvolume = \"/vol\"\ndevice = \"UUID=abc\"\n[[source.subvolumes]]\nname = \"@other\"\n"
    } else {
        ""
    };
    let text = format!(
        r#"[general]
version = "0.7.22"
install_prefix = "/usr"
db_path = "{db}"
btrbk_conf = "{conf}"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[[source]]
label = "s"
volume = "/vol"
device = "UUID=abc"
[[source.subvolumes]]
name = "@"
{s2}[[target]]
label = "t"
serial = "X"
mount = "/mnt/t"
role = "primary"
[target.retention]
daily = 7
[email]
enabled = false
[gui]
enabled = false
"#,
        db = dir.join("index.db").display(),
        conf = conf.display(),
    );
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn write_config(dir: &Path) -> std::path::PathBuf {
    write_config_with(dir, false)
}

fn btrdasd(args: &[&str]) -> std::process::Output {
    // `subvol sync` reads the index database for its coverage check; without
    // `--db` that is the host's. No test may reach it.
    assert!(
        !(args.starts_with(&["subvol", "sync"]) && !args.contains(&"--db")),
        "a test ran `subvol sync` without --db: {args:?}"
    );
    Command::new(env!("CARGO_BIN_EXE_btrdasd"))
        .args(args)
        .output()
        .expect("spawn btrdasd")
}

#[test]
fn subvol_add_and_remove_regenerate_btrbk_conf() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let config_s = config.to_str().unwrap();
    // The operator's comment and a key this binary does not know survive the
    // binary's save (bd DAS-Backup-Manager-28il).
    let hand = format!(
        "# operator note\nfuture_top = \"kept\"\n{}",
        std::fs::read_to_string(&config).unwrap()
    );
    std::fs::write(&config, &hand).unwrap();
    // A save that changes nothing writes in the backup-scoping keys the file
    // leaves out (and nothing else); that is the baseline the add and the
    // remove below must return to.
    let out = btrdasd(&["schedule", "set", "--config", config_s]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let baseline = std::fs::read_to_string(&config).unwrap();
    assert!(
        baseline.starts_with(&hand[..hand.find("[[source]]").unwrap()]),
        "{baseline}"
    );

    let out = btrdasd(&["subvol", "add", "s", "@home", "--config", config_s]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let conf = std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap();
    assert!(
        conf.contains("  subvolume             @home\n    snapshot_name       home\n"),
        "{conf}"
    );
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(
        saved.starts_with("# operator note\nfuture_top = \"kept\"\n"),
        "{saved}"
    );
    assert!(saved.contains("@home"), "{saved}");

    let out = btrdasd(&["subvol", "remove", "s", "@home", "--config", config_s]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let conf = std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap();
    assert!(!conf.contains("@home"), "{conf}");
    // Added, then removed: the operator's file is back as it was.
    assert_eq!(std::fs::read_to_string(&config).unwrap(), baseline);
}

/// `btrdasd schedule set` wrote `to_toml()` over the file with `fs::write`:
/// every comment and unknown key gone, and not in one step (bd
/// DAS-Backup-Manager-28il). It saves through `Config::save` now.
#[test]
fn schedule_set_keeps_unknown_keys_and_comments() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let config_s = config.to_str().unwrap();
    let hand = format!(
        "# operator note\nfuture_top = \"kept\" # why\n{}",
        std::fs::read_to_string(&config).unwrap()
    )
    .replacen("[schedule]\n", "[schedule]\nfuture_schedule = 1\n", 1);
    std::fs::write(&config, &hand).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();

    let out = btrdasd(&[
        "schedule",
        "set",
        "--incremental",
        "05:55",
        "--config",
        config_s,
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let saved = std::fs::read_to_string(&config).unwrap();
    for kept in [
        "# operator note",
        "future_top = \"kept\" # why",
        "future_schedule = 1",
    ] {
        assert!(saved.contains(kept), "lost {kept:?}:\n{saved}");
    }
    assert!(saved.contains("incremental = \"05:55\""), "{saved}");
    assert_eq!(
        std::fs::metadata(&config).unwrap().permissions().mode() & 0o7777,
        0o600
    );
}

/// A key a known table does not have (a typo the save keeps and nothing
/// reads) is warned about by `config validate`, with the nearest key; the
/// config is still valid (bd DAS-Backup-Manager-28il).
#[test]
fn config_validate_warns_about_an_unknown_key_with_a_hint() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let config_s = config.to_str().unwrap();
    let out = btrdasd(&["config", "validate", "--config", config_s]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Warning"),
        "a clean config warns nothing: {stderr}"
    );

    let typo = std::fs::read_to_string(&config).unwrap().replacen(
        "device = \"UUID=abc\"\n",
        "device = \"UUID=abc\"\ntarget_lables = [\"t\"]\n",
        1,
    );
    std::fs::write(&config, typo).unwrap();
    let out = btrdasd(&["config", "validate", "--config", config_s]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "Warning: source[0].target_lables is not a key this version reads \
             (did you mean target_labels?)"
        ),
        "{stderr}"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("Config is valid."));
}

/// bd DAS-Backup-Manager-9ry: `manual_only` is gone, and with it the two
/// commands that set it.
#[test]
fn subvol_set_manual_and_set_auto_are_no_longer_commands() {
    for action in ["set-manual", "set-auto"] {
        let out = btrdasd(&["subvol", action, "s", "@"]);
        assert_eq!(out.status.code(), Some(2), "{action}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("unrecognized subcommand"),
            "{action}: {stderr}"
        );
    }
}

#[test]
fn subvol_add_refuses_a_name_collision_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config_with(dir.path(), true);
    let config_s = config.to_str().unwrap();
    assert!(
        btrdasd(&["subvol", "add", "s", "a/b", "--config", config_s])
            .status
            .success()
    );
    let before = std::fs::read_to_string(&config).unwrap();
    let conf_before = std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap();

    let out = btrdasd(&["subvol", "add", "s2", "a-b", "--config", config_s]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("snapshot name 'a-b'"));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
        conf_before
    );
}

#[test]
fn subvol_sync_exits_one_when_a_volume_cannot_be_read_and_two_without_a_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    // /vol is not a mountpoint on any test machine.
    let db = dir.path().join("index.db");
    let out = btrdasd(&[
        "subvol",
        "sync",
        "--db",
        db.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("SUBVOLUME SYNC\n"), "{text}");
    // A failed sync still prints its coverage line, last: the script's
    // contract. Nothing on the unread volume was retired, so it is OK.
    assert!(
        text.ends_with(
            "\nSUBVOLUME COVERAGE: OK  (no earlier record: 1 subvolume/target pair — not \
             recorded (only a backup run records))\n"
        ),
        "{text}"
    );
    assert!(!db.exists(), "a compare-only sync created the database");
    assert!(text.contains("VOLUMES NOT READ"), "{text}");
    // The library names the source and device before the reason.
    assert!(
        text.contains("/vol: source 's' names device 'UUID=abc': /vol is not mounted"),
        "{text}"
    );

    let out = btrdasd(&[
        "subvol",
        "sync",
        "--db",
        dir.path().join("index.db").to_str().unwrap(),
        "--config",
        dir.path().join("none.toml").to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
}

/// bd DAS-Backup-Manager-sp78.3: `--json` was accepted and the text report
/// printed anyway. With no JSON form, the flag is refused before anything runs.
#[test]
fn subvol_sync_refuses_json_and_runs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let db = dir.path().join("index.db");
    let out = btrdasd(&[
        "subvol",
        "sync",
        "--db",
        db.to_str().unwrap(),
        "--config",
        config.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("subvol sync has no JSON output"), "{err}");
    assert!(
        out.stdout.is_empty(),
        "nothing ran: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn subvol_expire_prints_nothing_and_exits_zero_when_nothing_is_retired() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let out = btrdasd(&[
        "subvol",
        "expire",
        "--config",
        config.to_str().unwrap(),
        "--db",
        dir.path().join("index.db").to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty());
}

/// `subvol sync --dry-run --render-btrbk-conf` writes the btrbk.conf the real
/// run would leave into an existing file, and nothing else (bd
/// DAS-Backup-Manager-g17). `/vol` is not mounted here, so nothing is read,
/// the sync fails, and the real run would leave btrbk.conf as it is.
#[test]
fn sync_dry_run_renders_the_planned_btrbk_conf_into_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let config_s = config.to_str().unwrap();
    std::fs::write(dir.path().join("btrbk.conf"), "CURRENT\n").unwrap();
    let before = std::fs::read_to_string(&config).unwrap();
    let render = dir.path().join("planned.conf");
    std::fs::write(&render, "").unwrap();
    let render_s = render.to_str().unwrap();

    let out = btrdasd(&[
        "subvol",
        "sync",
        "--db",
        dir.path().join("index.db").to_str().unwrap(),
        "--config",
        config_s,
        "--dry-run",
        "--render-btrbk-conf",
        render_s,
    ]);
    assert_eq!(out.status.code(), Some(1), "the volume was not read");
    assert_eq!(std::fs::read_to_string(&render).unwrap(), "CURRENT\n");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("btrbk.conf")).unwrap(),
        "CURRENT\n"
    );

    // Not without --dry-run: a real sync writes the real file.
    std::fs::write(&render, "").unwrap();
    let out = btrdasd(&[
        "subvol",
        "sync",
        "--db",
        dir.path().join("index.db").to_str().unwrap(),
        "--config",
        config_s,
        "--render-btrbk-conf",
        render_s,
    ]);
    assert_eq!(out.status.code(), Some(2), "clap refuses the flag alone");
    assert_eq!(std::fs::read_to_string(&render).unwrap(), "");

    // Nothing readable would be left: no file to render, and the command fails.
    std::fs::remove_file(dir.path().join("btrbk.conf")).unwrap();
    let out = btrdasd(&[
        "subvol",
        "sync",
        "--db",
        dir.path().join("index.db").to_str().unwrap(),
        "--config",
        config_s,
        "--dry-run",
        "--render-btrbk-conf",
        render_s,
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no btrbk.conf to render"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&render).unwrap(), "");
}

/// Config with `[boot]` naming `@` and source `s` carrying `target_subdirs`
/// (`None` = the key absent), plus the btrbk.conf that names `@`'s snapshot.
fn boot_plan_config(dir: &Path, subdirs: Option<&str>) -> std::path::PathBuf {
    let config = write_config(dir);
    let mut text = std::fs::read_to_string(&config).unwrap();
    if let Some(list) = subdirs {
        let anchor = "device = \"UUID=abc\"\n";
        assert!(text.contains(anchor), "{text}");
        text = text.replacen(anchor, &format!("{anchor}target_subdirs = {list}\n"), 1);
    }
    std::fs::write(&config, format!("{text}[boot]\nsubvolumes = [\"@\"]\n")).unwrap();
    std::fs::write(
        dir.join("btrbk.conf"),
        "volume /vol\n  subvolume  @\n    snapshot_name  root-\n",
    )
    .unwrap();
    config
}

fn boot_plan_stdout(config: &Path) -> String {
    let out = btrdasd(&["backup", "boot-plan", "--config", config.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn backup_boot_plan_prints_the_subdirs_comma_joined_and_dash_when_there_are_none() {
    let one = tempfile::tempdir().unwrap();
    let config = boot_plan_config(one.path(), Some("[\"nvme\"]"));
    assert_eq!(boot_plan_stdout(&config), "@\troot-\tnvme\n");

    let two = tempfile::tempdir().unwrap();
    let config = boot_plan_config(two.path(), Some("[\"nvme\", \"ssd\"]"));
    assert_eq!(boot_plan_stdout(&config), "@\troot-\tnvme,ssd\n");

    let none = tempfile::tempdir().unwrap();
    let config = boot_plan_config(none.path(), None);
    assert_eq!(boot_plan_stdout(&config), "@\troot-\t-\n");
}

#[test]
fn backup_boot_plan_refuses_a_subdirectory_the_script_could_not_split() {
    let dir = tempfile::tempdir().unwrap();
    let config = boot_plan_config(dir.path(), Some("[\"a,b\"]"));
    let out = btrdasd(&["backup", "boot-plan", "--config", config.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("comma"));
}

#[test]
fn backup_boot_plan_refuses_an_unreadable_btrbk_conf() {
    let dir = tempfile::tempdir().unwrap();
    let config = boot_plan_config(dir.path(), Some("[\"nvme\"]"));
    std::fs::remove_file(dir.path().join("btrbk.conf")).unwrap();
    let out = btrdasd(&["backup", "boot-plan", "--config", config.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("btrbk.conf"), "{err}");
    assert!(out.stdout.is_empty());
}

/// A boot plan whose `[boot]` subvolume is `subvol_toml` (a TOML string
/// literal, quotes included), whose `btrbk.conf` carries `conf` verbatim and
/// whose source has `subdirs_toml` as `target_subdirs`.
fn boot_plan_refusal(subvol_toml: &str, conf: &str, subdirs_toml: &str) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let mut text = std::fs::read_to_string(&config).unwrap();
    let anchor = "device = \"UUID=abc\"\n";
    text = text.replacen(
        anchor,
        &format!("{anchor}target_subdirs = {subdirs_toml}\n"),
        1,
    );
    std::fs::write(
        &config,
        format!("{text}[boot]\nsubvolumes = [{subvol_toml}]\n"),
    )
    .unwrap();
    std::fs::write(dir.path().join("btrbk.conf"), conf).unwrap();
    btrdasd(&["backup", "boot-plan", "--config", config.to_str().unwrap()])
}

const GOOD_CONF: &str = "volume /vol\n  subvolume  @\n    snapshot_name  root-\n";

/// Exit 2, nothing on stdout, and stderr naming the subvolume and containing
/// `needle` (the field).
fn assert_refused(out: &std::process::Output, subvol: &str, needle: &str) {
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(err.contains(subvol), "{err}");
    assert!(err.contains(needle), "{err}");
}

#[test]
fn backup_boot_plan_refuses_a_subvolume_name_the_script_could_not_parse() {
    let out = boot_plan_refusal("\"\"", GOOD_CONF, "[\"nvme\"]");
    assert_refused(&out, "\"\"", "subvolume name");
    let out = boot_plan_refusal("\"a/b\"", GOOD_CONF, "[\"nvme\"]");
    assert_refused(&out, "a/b", "slash");
    let out = boot_plan_refusal("\"a b\"", GOOD_CONF, "[\"nvme\"]");
    assert_refused(&out, "a b", "whitespace");
    let out = boot_plan_refusal("\"a\\tb\"", GOOD_CONF, "[\"nvme\"]");
    assert_refused(&out, "a\\tb", "whitespace");
    let out = boot_plan_refusal("\"a\\nb\"", GOOD_CONF, "[\"nvme\"]");
    assert_refused(&out, "a\\nb", "whitespace");
}

#[test]
fn backup_boot_plan_refuses_a_snapshot_name_the_script_could_not_parse() {
    let conf = "volume /vol\n  subvolume  @\n    snapshot_name  root x\n";
    let out = boot_plan_refusal("\"@\"", conf, "[\"nvme\"]");
    assert_refused(&out, "@", "snapshot_name");
    let conf = "volume /vol\n  subvolume  @\n    snapshot_name  a\tb\n";
    let out = boot_plan_refusal("\"@\"", conf, "[\"nvme\"]");
    assert_refused(&out, "@", "snapshot_name");
    // A name of exactly `-` is the "absent" marker: ambiguous.
    let conf = "volume /vol\n  subvolume  @\n    snapshot_name  -\n";
    let out = boot_plan_refusal("\"@\"", conf, "[\"nvme\"]");
    assert_refused(&out, "@", "ambiguous");
}

#[test]
fn backup_boot_plan_refuses_a_subdirectory_that_is_empty_dash_or_has_whitespace() {
    for (subdirs, needle) in [
        ("[\"/\"]", "empty"),
        ("[\"nvme\", \"//\"]", "empty"),
        ("[\"-\"]", "ambiguous"),
        ("[\"a b\"]", "whitespace"),
        ("[\"a\\tb\"]", "whitespace"),
        ("[\"a\\nb\"]", "whitespace"),
    ] {
        let out = boot_plan_refusal("\"@\"", GOOD_CONF, subdirs);
        assert_refused(&out, "@", needle);
    }
}

/// `Config::load` does not run `validate`, so `boot-plan` enforces the boot
/// step's uniqueness and shape rules itself: exit 2, nothing on stdout.
#[test]
fn backup_boot_plan_refuses_repeated_or_dot_boot_subvolumes() {
    for (subvol_toml, subvol, needle) in [
        ("\"@\", \"@\"", "@", "more than once"),
        ("\"..\"", "..", "'.' or '..'"),
        ("\"@\", \".\"", "'.'", "'.' or '..'"),
    ] {
        let out = boot_plan_refusal(subvol_toml, GOOD_CONF, "[\"nvme\"]");
        assert_refused(&out, subvol, needle);
    }
}

#[test]
fn backup_boot_plan_refuses_two_targets_sharing_a_label_or_a_mount() {
    let second = "[[target]]\nlabel = \"{L}\"\nserial = \"Y\"\nmount = \"{M}\"\nrole = \"primary\"\n[target.retention]\ndaily = 7\n";
    for (label, mount, needle) in [
        ("t", "/mnt/u", "label 't'"),
        ("u", "/mnt/t", "mount '/mnt/t'"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = boot_plan_config(dir.path(), Some("[\"nvme\"]"));
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(&second.replace("{L}", label).replace("{M}", mount));
        std::fs::write(&config, text).unwrap();
        let out = btrdasd(&["backup", "boot-plan", "--config", config.to_str().unwrap()]);
        assert_refused(&out, "unique", needle);
    }
    // The control: distinct label and mount print the plan.
    let dir = tempfile::tempdir().unwrap();
    let config = boot_plan_config(dir.path(), Some("[\"nvme\"]"));
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&second.replace("{L}", "u").replace("{M}", "/mnt/u"));
    std::fs::write(&config, text).unwrap();
    assert_eq!(boot_plan_stdout(&config), "@\troot-\tnvme\n");
}

#[test]
fn backup_boot_plan_refuses_a_config_it_cannot_load() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.toml");
    let out = btrdasd(&["backup", "boot-plan", "--config", missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot read"), "{err}");
}

// --- a scratch --config must not reach the live btrbk files (bd DAS-Backup-Manager-5u15)

/// A config at a scratch path whose `[general]` says nothing about
/// `btrbk_conf`, so it defaults to the live `/etc/btrbk/btrbk.conf` — what a
/// copy of the host's config looks like.
fn write_scratch_config_with_live_outputs(dir: &Path) -> std::path::PathBuf {
    let text = std::fs::read_to_string(write_config(dir))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("btrbk_conf"))
        .collect::<Vec<_>>()
        .join("\n")
        // A second subvolume, so `remove` has one it may take (a source
        // cannot be left empty).
        .replacen(
            "name = \"@\"",
            "name = \"@\"\n[[source.subvolumes]]\nname = \"@extra\"",
            1,
        );
    let path = dir.join("scratch.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// Run as root these would be the very write they assert is refused.
fn running_as_root() -> bool {
    // SAFETY: geteuid() has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// `args --config <scratch naming the live outputs>` must be refused for those
/// outputs — not fail at the write — and leave the scratch config alone.
fn assert_refused_for_live_outputs(args: &[&str]) {
    if running_as_root() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let config = write_scratch_config_with_live_outputs(dir.path());
    let before = std::fs::read_to_string(&config).unwrap();
    let mut full = args.to_vec();
    full.extend(["--config", config.to_str().unwrap()]);
    let out = btrdasd(&full);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{args:?} must fail: {err}");
    assert!(
        err.contains("live /etc/btrbk"),
        "{args:?} must be refused for its live outputs, not fail at the write: {err}"
    );
    assert!(
        !err.contains("Permission denied"),
        "{args:?} reached the write: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        before,
        "{args:?} changed the scratch config"
    );
}

#[test]
fn subvol_add_refuses_a_scratch_config_naming_the_live_btrbk_files() {
    assert_refused_for_live_outputs(&["subvol", "add", "s", "@home"]);
}

#[test]
fn subvol_remove_refuses_a_scratch_config_naming_the_live_btrbk_files() {
    assert_refused_for_live_outputs(&["subvol", "remove", "s", "@extra"]);
}

#[test]
fn a_real_subvol_sync_refuses_a_scratch_config_naming_the_live_btrbk_files() {
    assert_refused_for_live_outputs(&["subvol", "sync", "--db", "/nonexistent/no-index.db"]);
}

#[test]
fn a_dry_run_sync_of_a_scratch_config_is_not_refused_because_it_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_scratch_config_with_live_outputs(dir.path());
    let out = btrdasd(&[
        "subvol",
        "sync",
        "--db",
        dir.path().join("index.db").to_str().unwrap(),
        "--dry-run",
        "--config",
        config.to_str().unwrap(),
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("refusing"), "{err}");
}

#[test]
fn a_scratch_config_that_names_its_own_btrbk_files_still_saves() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let out = btrdasd(&[
        "subvol",
        "add",
        "s",
        "@home",
        "--config",
        config.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join("btrbk.conf").exists());
}

// --- subvolume coverage (bd DAS-Backup-Manager-sp78.1) -----------------------

/// A coverage record in `dir`: `@a` covered, `@b` dropped, both to `big`.
fn seeded_coverage_db(dir: &Path) -> std::path::PathBuf {
    use buttered_dasd::db::{CoverageRow, CoverageState, Database};
    let path = dir.join("index.db");
    let row = |subvolume: &str, dropped: bool| CoverageRow {
        volume: "/ssd".into(),
        subvolume: subvolume.into(),
        target: "big".into(),
        target_identity: None,
        source_label: "ssd".into(),
        first_covered: "2026-10-01".into(),
        last_covered: "2026-10-01".into(),
        dropped_since: dropped.then(|| "2026-10-02".into()),
        dropped_checks: u32::from(dropped),
        drop_rule: dropped.then(|| "excluded by pattern '@b'".into()),
        subvol_uuid: None,
        volume_uuid: None,
    };
    Database::open(&path)
        .unwrap()
        .record_coverage(|_| {
            (
                (),
                vec![row("@a", false), row("@b", true)],
                CoverageState {
                    first_recorded: "2026-10-01".into(),
                    last_recorded: "2026-10-02".into(),
                    checks: 2,
                },
            )
        })
        .unwrap();
    path
}

/// `--record-coverage` records the check, and only with it: the sync fails
/// here (`/vol` is not mounted), and the check is still made and kept.
#[test]
fn only_record_coverage_writes_the_coverage_record() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let db = dir.path().join("index.db");
    let sync = |extra: &[&str]| {
        let mut args = vec![
            "subvol",
            "sync",
            "--config",
            config.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
        ];
        args.extend(extra);
        btrdasd(&args)
    };
    let out = sync(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        !db.exists(),
        "a sync without --record-coverage created the database"
    );

    let out = sync(&["--record-coverage"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "coverage leaves the sync's exit code alone"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.ends_with(
            "\nSUBVOLUME COVERAGE: OK  (no earlier record: 1 subvolume/target pair; recorded — \
             drops are reported from the next run)\n"
        ),
        "{text}"
    );
    let recorded = buttered_dasd::db::read_coverage_read_only(&db)
        .unwrap()
        .expect("recorded");
    assert_eq!(recorded.state.checks, 1);
    assert_eq!(recorded.rows.len(), 1);
}

#[test]
fn recording_coverage_and_a_dry_run_cannot_be_asked_together() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path());
    let db = dir.path().join("index.db");
    let out = btrdasd(&[
        "subvol",
        "sync",
        "--config",
        config.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--dry-run",
        "--record-coverage",
    ]);
    assert_eq!(out.status.code(), Some(2), "clap refuses the pair");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot be used with"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!db.exists());
}

#[test]
fn coverage_accept_exits_by_what_it_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_coverage_db(dir.path());
    let db_s = db.to_str().unwrap();
    let read = || buttered_dasd::db::read_coverage_read_only(&db).unwrap();
    let before = read();

    let out = btrdasd(&["subvol", "coverage", "accept", "/ssd", "@a", "--db", db_s]);
    assert_eq!(out.status.code(), Some(1), "a covered pair");
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("not dropped: nothing accepted"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(read(), before, "nothing changed");

    let out = btrdasd(&["subvol", "coverage", "accept", "/ssd", "@b", "--db", db_s]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .starts_with("Accepted 1 pair; they are no longer tracked:\n  /ssd  @b  -> big"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(read().unwrap().rows.len(), 1);

    let missing = dir.path().join("missing.db");
    let out = btrdasd(&[
        "subvol",
        "coverage",
        "accept",
        "--all",
        "--db",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!missing.exists(), "accept created a database");

    // --all and a pair are one or the other; one of them is required.
    let out = btrdasd(&[
        "subvol", "coverage", "accept", "/ssd", "@b", "--all", "--db", db_s,
    ]);
    assert_eq!(out.status.code(), Some(2));
    let out = btrdasd(&["subvol", "coverage", "accept", "--db", db_s]);
    assert_eq!(out.status.code(), Some(2));
    let out = btrdasd(&[
        "subvol", "coverage", "accept", "--all", "--target", "big", "--db", db_s,
    ]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn coverage_list_prints_the_dropped_pairs_and_the_accept_log_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let db = seeded_coverage_db(dir.path());
    let db_s = db.to_str().unwrap();
    let out = btrdasd(&["subvol", "coverage", "list", "--db", db_s, "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["dropped"][0]["subvolume"], "@b");
    assert_eq!(v["accepted"].as_array().unwrap().len(), 0);

    btrdasd(&["subvol", "coverage", "accept", "--all", "--db", db_s]);
    let out = btrdasd(&["subvol", "coverage", "list", "--db", db_s]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("  No longer backed up: none\n"), "{text}");
    assert!(
        text.contains("  /ssd  @b  -> big  (no longer backed up since 2026-10-02"),
        "{text}"
    );

    let out = btrdasd(&[
        "subvol",
        "coverage",
        "list",
        "--db",
        dir.path().to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(2), "a directory is no database");
}
