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

pub fn run(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !nix_is_root() {
        eprintln!("Error: btrdasd setup requires root privileges.");
        eprintln!("Run: sudo btrdasd setup");
        std::process::exit(1);
    }

    if args.check {
        installer::check()?;
    } else if args.uninstall {
        let remove_db = if args.force {
            false
        } else {
            dialoguer::Confirm::new()
                .with_prompt("Also remove the backup database?")
                .default(false)
                .interact()?
        };
        installer::uninstall(remove_db)?;
    } else if args.uninstall_all {
        let remove_db = if args.force {
            false
        } else {
            dialoguer::Confirm::new()
                .with_prompt("Also remove the backup database?")
                .default(false)
                .interact()?
        };
        installer::uninstall_all(remove_db)?;
    } else if args.upgrade {
        installer::upgrade()?;
    } else if args.force {
        // Non-interactive install: requires existing config
        let config_path = std::path::PathBuf::from("/etc/das-backup/config.toml");
        if !config_path.exists() {
            return Err(
                "Cannot run non-interactive install: no existing config found. \
                 Run the interactive wizard first, or use --upgrade to regenerate."
                    .into(),
            );
        }
        let config = config::Config::load(&config_path)?;
        installer::install(&config)?;
    } else {
        // Fresh install or --modify
        let existing = if args.modify {
            load_existing_for_modify(&std::path::PathBuf::from("/etc/das-backup/config.toml"))?
        } else {
            None
        };

        let sys = detect::SystemInfo::detect();
        let config = wizard::run_wizard(&sys, existing)?;
        installer::install(&config)?;
    }

    Ok(())
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

fn nix_is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
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
            Ok(_) => panic!("an unparseable existing config must be an error, not Ok(None)"),
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
}
