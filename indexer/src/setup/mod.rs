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
            config::Config::load(&std::path::PathBuf::from("/etc/das-backup/config.toml")).ok()
        } else {
            None
        };

        let sys = detect::SystemInfo::detect();
        let config = wizard::run_wizard(&sys, existing)?;
        installer::install(&config)?;
    }

    Ok(())
}

fn nix_is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}
