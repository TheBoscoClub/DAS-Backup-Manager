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

    /// Validate config + deps, report issues, change nothing
    #[arg(long)]
    pub check: bool,
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
        let remove_db = dialoguer::Confirm::new()
            .with_prompt("Also remove the backup database?")
            .default(false)
            .interact()?;
        installer::uninstall(remove_db)?;
    } else if args.upgrade {
        installer::upgrade()?;
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
