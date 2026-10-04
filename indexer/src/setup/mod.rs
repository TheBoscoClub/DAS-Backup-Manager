pub mod config;
pub mod detect;
pub mod env_export;
pub mod installer;
pub mod templates;
pub mod wizard;

use std::path::Path;

use clap::Args;

#[derive(Args)]
pub struct SetupArgs {
    /// Re-open wizard with current config pre-filled
    #[arg(long)]
    pub modify: bool,

    /// Regenerate files from existing config (after binary update)
    #[arg(long)]
    pub upgrade: bool,

    /// Remove all generated files, disable timers, optionally remove DB
    #[arg(long)]
    pub uninstall: bool,

    /// Remove ALL files: generated configs, binaries, D-Bus, polkit, icons, man page, completions
    #[arg(long)]
    pub uninstall_all: bool,

    /// Validate config + deps, report issues, change nothing
    #[arg(long)]
    pub check: bool,

    /// Non-interactive mode: skip all prompts, never remove/overwrite the backup database
    #[arg(long)]
    pub force: bool,
}

/// Where `btrdasd setup` reads and writes the host's configuration.
const CONFIG_PATH: &str = "/etc/das-backup/config.toml";

pub fn run(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    if let Err(msg) = require_root(effective_uid()) {
        eprintln!("{msg}");
        std::process::exit(1);
    }
    let wizard = |existing| {
        let sys = detect::SystemInfo::detect();
        wizard::run_wizard(&sys, existing)
    };
    let ask_remove_db = || -> Result<bool, Box<dyn std::error::Error>> {
        Ok(dialoguer::Confirm::new()
            .with_prompt("Also remove the backup database?")
            .default(false)
            .interact()?)
    };
    let host = SetupHost {
        site: installer::SetupLockSite::production(),
        check: &installer::check,
        install: &installer::install,
        uninstall: &installer::uninstall,
        uninstall_all: &installer::uninstall_all,
        upgrade: &installer::upgrade,
        wizard: &wizard,
        ask_remove_db: &ask_remove_db,
    };
    let outcome = dispatch(
        &args,
        Path::new(CONFIG_PATH),
        &host,
        &mut |line| println!("{line}"),
        &mut |line| eprintln!("{line}"),
    )?;
    match installer::exit_status(outcome) {
        0 => Ok(()),
        code => installer::exit_with(code),
    }
}

/// `--uninstall` or `--uninstall-all` under setup's locks, told whether to
/// remove the database too.
type Uninstaller<'a> =
    &'a dyn Fn(bool, &installer::SetupLocks) -> Result<(), Box<dyn std::error::Error>>;

/// `--upgrade`: the lock site, the config path, then `say` and `warn`.
type UpgradeMode<'a> = &'a dyn Fn(
    &installer::SetupLockSite,
    &Path,
    &mut dyn FnMut(String),
    &mut dyn FnMut(String),
) -> Result<installer::SetupOutcome, Box<dyn std::error::Error>>;

/// What `btrdasd setup` does to the host, one seam per step: the host's own
/// in production ([`run`]), scratch files and recorders in tests.
struct SetupHost<'a> {
    /// Where setup's two locks are.
    site: installer::SetupLockSite,
    /// `--check`: read-only, so it takes no lock.
    check: &'a dyn Fn() -> Result<(), Box<dyn std::error::Error>>,
    /// Regenerate and install every file for a config.
    install: installer::Installer<'a>,
    /// `--uninstall`, told whether to remove the database too.
    uninstall: Uninstaller<'a>,
    /// `--uninstall-all`, likewise.
    uninstall_all: Uninstaller<'a>,
    /// `--upgrade`, which takes the locks itself.
    upgrade: UpgradeMode<'a>,
    /// The interactive wizard, given the config to pre-fill.
    wizard:
        &'a dyn Fn(Option<config::Config>) -> Result<config::Config, Box<dyn std::error::Error>>,
    /// "Also remove the backup database?"
    ask_remove_db: &'a dyn Fn() -> Result<bool, Box<dyn std::error::Error>>,
}

/// What one `btrdasd setup` invocation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Check,
    Uninstall,
    UninstallAll,
    Upgrade,
    /// `--force` alone: reinstall from the existing config, no prompts.
    ForceInstall,
    Wizard {
        modify: bool,
    },
}

/// Pick the action for a flag combination.
///
/// The flags are not mutually exclusive on the command line, so the order here
/// is the contract: the read-only `--check` outranks everything, and `--force`
/// is an install of its own only when no other mode is named — next to
/// `--uninstall`, `--uninstall-all` or `--upgrade` it is just their "no
/// prompts" modifier.
fn select_action(args: &SetupArgs) -> Action {
    if args.check {
        Action::Check
    } else if args.uninstall {
        Action::Uninstall
    } else if args.uninstall_all {
        Action::UninstallAll
    } else if args.upgrade {
        Action::Upgrade
    } else if args.force {
        Action::ForceInstall
    } else {
        Action::Wizard {
            modify: args.modify,
        }
    }
}

/// Do what `args` asks, with the config at `config_path` and the host as
/// `host` binds it; progress to `say`, a refusal to `warn`. Every mode that
/// writes or removes installed files does so under setup's two locks, taken
/// only once its questions are answered: held across a prompt, the backup
/// singleton would make a backup the timer starts meanwhile skip its run.
/// `--check` takes none.
fn dispatch(
    args: &SetupArgs,
    config_path: &Path,
    host: &SetupHost,
    say: &mut dyn FnMut(String),
    warn: &mut dyn FnMut(String),
) -> Result<installer::SetupOutcome, Box<dyn std::error::Error>> {
    // What the change made under the locks: `Ran(Err(why))` is a refusal
    // found only once they were held.
    let ran = match select_action(args) {
        Action::Check => {
            (host.check)()?;
            return Ok(installer::SetupOutcome::Done);
        }
        Action::Upgrade => return (host.upgrade)(&host.site, config_path, say, warn),
        Action::Uninstall => {
            let remove_db = remove_db_decision(args.force, host.ask_remove_db)?;
            installer::under_setup_locks(&host.site, "btrdasd setup --uninstall", |held| {
                (host.uninstall)(remove_db, held).map(Ok)
            })?
        }
        Action::UninstallAll => {
            let remove_db = remove_db_decision(args.force, host.ask_remove_db)?;
            installer::under_setup_locks(&host.site, "btrdasd setup --uninstall-all", |held| {
                (host.uninstall_all)(remove_db, held).map(Ok)
            })?
        }
        Action::ForceInstall => {
            installer::under_setup_locks(&host.site, "btrdasd setup --force", |held| {
                // Read under the locks: nothing can change it between this
                // read and the write.
                let config = load_existing_for_force(config_path)?;
                (host.install)(&config, held).map(Ok)
            })?
        }
        Action::Wizard { modify } => {
            let mut read = if modify {
                Some(load_existing_for_modify(config_path)?)
            } else {
                None
            };
            let existing = read.as_mut().and_then(|r| r.config.take());
            let config = (host.wizard)(existing)?;
            let job = if modify {
                "btrdasd setup --modify"
            } else {
                "btrdasd setup"
            };
            installer::under_setup_locks(&host.site, job, |held| {
                // The wizard pre-filled what --modify read; anything written
                // to the file since — the GUI, a subvol sync — would be lost
                // under the answers. Compared under the locks, so nothing
                // can land between this read and the write.
                if let Some(read) = &read
                    && config_bytes(config_path)? != read.bytes
                {
                    return Ok(Err(config_changed_line(config_path)));
                }
                (host.install)(&config, held).map(Ok)
            })?
        }
    };
    let why = match ran {
        installer::Locked::Ran(Ok(())) => return Ok(installer::SetupOutcome::Done),
        installer::Locked::Ran(Err(why)) | installer::Locked::Refused(why) => why,
    };
    warn(why);
    Ok(installer::SetupOutcome::Refused)
}

/// What `--modify` says, and stops at, when `config.toml` at `path` changed
/// while the wizard was open.
fn config_changed_line(path: &Path) -> String {
    format!(
        "Refused, nothing written or removed: {} changed while the wizard was open — something \
         else wrote it after --modify read it. Run setup --modify again to start from it as it \
         is now (exit {}).",
        path.display(),
        installer::SETUP_REFUSED_EXIT
    )
}

/// The bytes of the file at `path`, or `None` when there is none. Any other
/// failure to read it is an error: a change that cannot be ruled out is not
/// ruled out.
fn config_bytes(path: &Path) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display()).into()),
    }
}

/// Whether an uninstall also removes the backup database.
///
/// `--force` never does, and never asks: it is the non-interactive mode, and
/// the database is the one thing an unattended uninstall must not be able to
/// take with it. Otherwise the operator's answer stands, and a prompt that
/// could not be shown stops the uninstall rather than standing in for one.
fn remove_db_decision<E>(force: bool, ask: impl FnOnce() -> Result<bool, E>) -> Result<bool, E> {
    if force { Ok(false) } else { ask() }
}

/// Load the config a non-interactive (`--force`) install regenerates from.
///
/// There is no wizard to fall back on, so a missing config is an error rather
/// than a reason to install defaults.
fn load_existing_for_force(
    path: &std::path::Path,
) -> Result<config::Config, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Err(
            "Cannot run non-interactive install: no existing config found. \
             Run the interactive wizard first, or use --upgrade to regenerate."
                .into(),
        );
    }
    config::Config::load(path)
}

/// `config.toml` as `--modify` found it before the wizard opened: the bytes it
/// read — `None` when there was no file — and the config parsed from them.
struct ConfigAsRead {
    bytes: Option<Vec<u8>>,
    config: Option<config::Config>,
}

/// Load the config `--modify` is meant to pre-fill the wizard with, keeping
/// the bytes it was parsed from, so that a change to the file while the wizard
/// is open can be told once setup holds its locks.
///
/// No config there yet means only that: the wizard starts from defaults. A
/// config that EXISTS but cannot be read is an error and stops the run. It
/// used to be `Config::load(..).ok()`, which collapsed both cases into `None`:
/// a config with one bad line sent the wizard to its defaults and
/// `installer::install` then wrote those defaults straight over the file the
/// operator asked to modify — every target, serial and retention setting gone,
/// with nothing printed (bd DAS-Backup-Manager-8wx).
fn load_existing_for_modify(path: &Path) -> Result<ConfigAsRead, Box<dyn std::error::Error>> {
    let refusing = |e: &dyn std::fmt::Display| -> Box<dyn std::error::Error> {
        format!(
            "--modify: refusing to continue — the existing config {} could not be read ({e}). \
             Fix or move it first; continuing would overwrite it with defaults.",
            path.display()
        )
        .into()
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfigAsRead {
                bytes: None,
                config: None,
            });
        }
        Err(e) => return Err(refusing(&e)),
    };
    let text = std::str::from_utf8(&bytes).map_err(|e| refusing(&e))?;
    let config = config::Config::from_toml(text).map_err(|e| refusing(&e))?;
    Ok(ConfigAsRead {
        bytes: Some(bytes),
        config: Some(config),
    })
}

/// Refuse to run setup as anyone but root: every mode reads or writes
/// root-owned paths, and failing halfway through an install is worse than not
/// starting.
fn require_root(euid: u32) -> Result<(), &'static str> {
    if euid == 0 {
        Ok(())
    } else {
        Err("Error: btrdasd setup requires root privileges.\nRun: sudo btrdasd setup")
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid() has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The three cases `--modify` has to tell apart. Before the fix the middle
    /// one was indistinguishable from the first.
    #[test]
    fn modify_refuses_an_unreadable_existing_config() {
        let dir = tempfile::tempdir().unwrap();

        // 1. No config yet -> start the wizard from defaults.
        let missing = dir.path().join("absent.toml");
        let read = load_existing_for_modify(&missing).unwrap();
        assert!(
            read.config.is_none() && read.bytes.is_none(),
            "a config that does not exist must be no config, read from no file"
        );

        // 2. A config that exists but will not parse -> ERROR, never Ok(None).
        //    Ok(None) here means the wizard starts from defaults and then
        //    overwrites this very file with them.
        let broken = dir.path().join("broken.toml");
        let mut f = std::fs::File::create(&broken).unwrap();
        writeln!(f, "this is not = = valid toml [[[").unwrap();
        drop(f);
        let err = match load_existing_for_modify(&broken) {
            Err(e) => e,
            Ok(_) => panic!("an unparsable existing config must be an error, not Ok(None)"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("could not be read") && msg.contains("broken.toml"),
            "error should name the file and the reason, got: {msg}"
        );

        // 2b. A config that cannot be read at all — here a directory — is an
        //     error too, never "no config yet".
        let unreadable = dir.path().join("a-directory.toml");
        std::fs::create_dir(&unreadable).unwrap();
        let msg = match load_existing_for_modify(&unreadable) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an unreadable existing config must be an error"),
        };
        assert!(
            msg.contains("could not be read") && msg.contains("a-directory.toml"),
            "got: {msg}"
        );

        // 3. Positive control: a valid config still loads, so the guard cannot
        //    be passing by refusing everything.
        let good = dir.path().join("good.toml");
        let cfg = config::Config::default();
        cfg.save(&good).unwrap();
        let read = load_existing_for_modify(&good).expect("a valid config must load");
        let loaded = read.config.expect("a valid config must be Some");
        assert_eq!(loaded.general.db_path, cfg.general.db_path);
        assert_eq!(
            read.bytes,
            Some(std::fs::read(&good).unwrap()),
            "the bytes it was parsed from, to compare under the locks"
        );
    }

    fn args(flags: &[&str]) -> SetupArgs {
        let mut a = SetupArgs {
            modify: false,
            upgrade: false,
            uninstall: false,
            uninstall_all: false,
            check: false,
            force: false,
        };
        for f in flags {
            match *f {
                "modify" => a.modify = true,
                "upgrade" => a.upgrade = true,
                "uninstall" => a.uninstall = true,
                "uninstall_all" => a.uninstall_all = true,
                "check" => a.check = true,
                "force" => a.force = true,
                other => panic!("unknown flag {other}"),
            }
        }
        a
    }

    #[test]
    fn only_uid_zero_may_run_setup() {
        assert_eq!(require_root(0), Ok(()));
        for euid in [1, 952, 1000, u32::MAX] {
            let msg = require_root(euid).expect_err("a non-root uid must be refused");
            assert!(msg.contains("requires root privileges"), "got: {msg}");
            assert!(msg.contains("sudo btrdasd setup"), "got: {msg}");
        }
    }

    /// Checked against an independent reading of the same fact, so a wrong
    /// answer cannot agree with itself.
    #[test]
    fn effective_uid_matches_what_the_kernel_reports() {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        // "Uid:\t<real>\t<effective>\t<saved>\t<fs>"
        let from_proc: u32 = status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|rest| rest.split_whitespace().nth(1))
            .expect("an effective uid in /proc/self/status")
            .parse()
            .unwrap();
        assert_eq!(effective_uid(), from_proc);
    }

    #[test]
    fn each_flag_selects_its_own_action() {
        let cases: &[(&[&str], Action)] = &[
            (&[], Action::Wizard { modify: false }),
            (&["modify"], Action::Wizard { modify: true }),
            (&["check"], Action::Check),
            (&["uninstall"], Action::Uninstall),
            (&["uninstall_all"], Action::UninstallAll),
            (&["upgrade"], Action::Upgrade),
            (&["force"], Action::ForceInstall),
        ];
        for (flags, want) in cases {
            assert_eq!(select_action(&args(flags)), *want, "flags: {flags:?}");
        }
    }

    #[test]
    fn combined_flags_resolve_toward_the_least_destructive_mode() {
        let cases: &[(&[&str], Action)] = &[
            // --check changes nothing, and stays that way whatever it is
            // combined with.
            (
                &[
                    "check",
                    "uninstall",
                    "uninstall_all",
                    "upgrade",
                    "force",
                    "modify",
                ],
                Action::Check,
            ),
            // --force beside another mode is that mode without prompts, never
            // a reinstall.
            (&["uninstall", "force"], Action::Uninstall),
            (&["uninstall_all", "force"], Action::UninstallAll),
            (&["upgrade", "force"], Action::Upgrade),
            // --uninstall keeps the installed binaries; it wins over the
            // variant that removes them.
            (&["uninstall", "uninstall_all"], Action::Uninstall),
            (&["uninstall_all", "upgrade"], Action::UninstallAll),
            // --force is non-interactive: it must not open the wizard.
            (&["force", "modify"], Action::ForceInstall),
        ];
        for (flags, want) in cases {
            assert_eq!(select_action(&args(flags)), *want, "flags: {flags:?}");
        }
    }

    #[test]
    fn forced_uninstall_never_removes_the_database_and_never_prompts() {
        let ask = || -> Result<bool, String> { panic!("--force must not prompt") };
        assert_eq!(remove_db_decision(true, ask), Ok(false));
    }

    #[test]
    fn interactive_uninstall_follows_the_operators_answer() {
        assert_eq!(
            remove_db_decision(false, || Ok::<_, String>(true)),
            Ok(true)
        );
        assert_eq!(
            remove_db_decision(false, || Ok::<_, String>(false)),
            Ok(false)
        );
        // No answer is not a "no": the uninstall stops instead.
        assert_eq!(
            remove_db_decision(false, || Err::<bool, _>("not a terminal".to_string())),
            Err("not a terminal".to_string())
        );
    }

    #[test]
    fn force_install_needs_an_existing_config() {
        let dir = tempfile::tempdir().unwrap();

        let missing = dir.path().join("absent.toml");
        let msg = match load_existing_for_force(&missing) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a missing config must not install defaults"),
        };
        assert!(msg.contains("no existing config found"), "got: {msg}");

        // Positive control: the config that is there is the one installed from.
        let good = dir.path().join("good.toml");
        let mut cfg = config::Config::default();
        cfg.general.db_path = "/var/lib/das-backup/force-test.db".to_string();
        cfg.save(&good).unwrap();
        let loaded = load_existing_for_force(&good).expect("an existing config must load");
        assert_eq!(loaded.general.db_path, cfg.general.db_path);
    }

    type BoxError = Box<dyn std::error::Error>;

    fn never_check() -> Result<(), BoxError> {
        panic!("--check must not run")
    }
    fn never_install(_: &config::Config, _: &installer::SetupLocks) -> Result<(), BoxError> {
        panic!("nothing may be installed")
    }
    fn never_uninstall(_: bool, _: &installer::SetupLocks) -> Result<(), BoxError> {
        panic!("nothing may be removed")
    }
    fn never_upgrade(
        _: &installer::SetupLockSite,
        _: &Path,
        _: &mut dyn FnMut(String),
        _: &mut dyn FnMut(String),
    ) -> Result<installer::SetupOutcome, BoxError> {
        panic!("nothing may be upgraded")
    }
    fn never_wizard(_: Option<config::Config>) -> Result<config::Config, BoxError> {
        panic!("the wizard must not open")
    }
    fn never_ask() -> Result<bool, BoxError> {
        panic!("nothing may be asked")
    }

    /// A host on which no step may run, its lock files in `dir`.
    fn host_that_runs_nothing(dir: &Path) -> SetupHost<'static> {
        SetupHost {
            site: installer::SetupLockSite {
                backup: dir.join("das-backup.lock"),
                maintenance: dir.join("das-maintenance.lock"),
            },
            check: &never_check,
            install: &never_install,
            uninstall: &never_uninstall,
            uninstall_all: &never_uninstall,
            upgrade: &never_upgrade,
            wizard: &never_wizard,
            ask_remove_db: &never_ask,
        }
    }

    #[test]
    fn config_bytes_tells_no_file_from_one_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "x = 1\n").unwrap();
        assert_eq!(config_bytes(&file).unwrap(), Some(b"x = 1\n".to_vec()));
        assert_eq!(config_bytes(&dir.path().join("absent.toml")).unwrap(), None);
        // A file that is there but cannot be read is not "no file": a change
        // that cannot be ruled out is not ruled out.
        let err = config_bytes(dir.path()).unwrap_err().to_string();
        assert!(
            err.starts_with(&format!("cannot read {}: ", dir.path().display())),
            "{err}"
        );
    }

    /// Both refusals happen before anything is installed, so these run
    /// without touching the host.
    #[test]
    fn dispatch_stops_before_installing_when_the_config_is_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let host = host_that_runs_nothing(dir.path());
        let mut quiet = |_: String| {};

        let missing = dir.path().join("absent.toml");
        let err =
            dispatch(&args(&["force"]), &missing, &host, &mut |_| {}, &mut quiet).unwrap_err();
        assert!(err.to_string().contains("no existing config found"));

        let broken = dir.path().join("broken.toml");
        std::fs::write(&broken, "this is not = = valid toml [[[\n").unwrap();
        let err =
            dispatch(&args(&["modify"]), &broken, &host, &mut |_| {}, &mut quiet).unwrap_err();
        assert!(err.to_string().contains("--modify: refusing to continue"));
    }
}
