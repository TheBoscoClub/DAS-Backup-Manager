pub mod config;

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
    // Root check
    if !nix_is_root() {
        eprintln!("Error: btrdasd setup requires root privileges.");
        eprintln!("Run: sudo btrdasd setup");
        std::process::exit(1);
    }

    if args.check {
        println!("setup --check: not yet implemented");
    } else if args.uninstall {
        println!("setup --uninstall: not yet implemented");
    } else if args.upgrade {
        println!("setup --upgrade: not yet implemented");
    } else {
        // Fresh install or --modify
        println!("setup wizard: not yet implemented");
    }
    Ok(())
}

fn nix_is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}
