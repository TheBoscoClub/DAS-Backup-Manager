pub mod config;
pub mod detect;
pub mod env_export;
pub mod installer;
pub mod templates;
pub mod wizard;

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
    dispatch(&args, std::path::Path::new(CONFIG_PATH))
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

fn dispatch(
    args: &SetupArgs,
    config_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let ask_remove_db = || {
        dialoguer::Confirm::new()
            .with_prompt("Also remove the backup database?")
            .default(false)
            .interact()
    };

    match select_action(args) {
        Action::Check => installer::check()?,
        Action::Uninstall => installer::uninstall(remove_db_decision(args.force, ask_remove_db)?)?,
        Action::UninstallAll => {
            installer::uninstall_all(remove_db_decision(args.force, ask_remove_db)?)?
        }
        Action::Upgrade => installer::upgrade()?,
        Action::ForceInstall => {
            let config = load_existing_for_force(config_path)?;
            installer::install(&config)?;
        }
        Action::Wizard { modify } => {
            let existing = if modify {
                load_existing_for_modify(config_path)?
            } else {
                None
            };

            let sys = detect::SystemInfo::detect();
            let config = wizard::run_wizard(&sys, existing)?;
            installer::install(&config)?;
        }
    }

    Ok(())
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

/// Load the config `--modify` is meant to pre-fill the wizard with.
///
/// `Ok(None)` means only one thing: there is no config there yet, so the wizard
/// starts from defaults. A config that EXISTS but cannot be read is an error and
/// stops the run. It used to be `Config::load(..).ok()`, which collapsed both
/// cases into `None`: a config with one bad line sent the wizard to its defaults
/// and `installer::install` then wrote those defaults straight over the file the
/// operator asked to modify — every target, serial and retention setting gone,
/// with nothing printed (bd DAS-Backup-Manager-8wx).
fn load_existing_for_modify(
    path: &std::path::Path,
) -> Result<Option<config::Config>, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(None);
    }
    match config::Config::load(path) {
        Ok(cfg) => Ok(Some(cfg)),
        Err(e) => Err(format!(
            "--modify: refusing to continue — the existing config {} could not be read ({e}). \
             Fix or move it first; continuing would overwrite it with defaults.",
            path.display()
        )
        .into()),
    }
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
        assert!(
            load_existing_for_modify(&missing).unwrap().is_none(),
            "a config that does not exist must be Ok(None)"
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

        // 3. Positive control: a valid config still loads, so the guard cannot
        //    be passing by refusing everything.
        let good = dir.path().join("good.toml");
        let cfg = config::Config::default();
        cfg.save(&good).unwrap();
        let loaded = load_existing_for_modify(&good)
            .expect("a valid config must load")
            .expect("a valid config must be Some");
        assert_eq!(loaded.general.db_path, cfg.general.db_path);
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

    /// Both refusals happen before the installer is reached, so these run
    /// without touching the host.
    #[test]
    fn dispatch_stops_before_installing_when_the_config_is_unusable() {
        let dir = tempfile::tempdir().unwrap();

        let missing = dir.path().join("absent.toml");
        let err = dispatch(&args(&["force"]), &missing).unwrap_err();
        assert!(err.to_string().contains("no existing config found"));

        let broken = dir.path().join("broken.toml");
        std::fs::write(&broken, "this is not = = valid toml [[[\n").unwrap();
        let err = dispatch(&args(&["modify"]), &broken).unwrap_err();
        assert!(err.to_string().contains("--modify: refusing to continue"));
    }
}
