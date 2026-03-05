// Interactive 10-step setup wizard using dialoguer prompts and console styling.
// Binary-only module (main.rs scope). No unit tests — testing via `btrdasd setup` on VM.
// Functions are consumed by the run() orchestrator in Task 6.
#![allow(dead_code)]

use console::style;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use crate::setup::config::*;
use crate::setup::detect::*;

/// Run the interactive setup wizard. Takes detected system info and an optional
/// existing config (for --modify mode). Returns a completed, validated Config.
pub fn run_wizard(
    sys: &SystemInfo,
    existing: Option<Config>,
) -> Result<Config, Box<dyn std::error::Error>> {
    let mut config = existing.unwrap_or_default();

    println!("\n{}", style("ButteredDASD Setup Wizard").bold().cyan());
    println!("{}\n", style("─".repeat(40)).dim());

    step_dependencies(sys)?;
    step_subvolumes(sys, &mut config)?;
    step_targets(sys, &mut config)?;
    step_esp(sys, &mut config)?;
    step_retention(&mut config)?;
    step_scheduling(sys, &mut config)?;
    step_email(&mut config)?;
    step_install_location(&mut config)?;
    step_gui(&mut config)?;
    step_review(&config)?;

    Ok(config)
}

// ---------------------------------------------------------------------------
// Step 1: Dependencies
// ---------------------------------------------------------------------------

fn step_dependencies(sys: &SystemInfo) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{} {}",
        style("[1/10]").bold().cyan(),
        style("Checking Dependencies").bold()
    );
    println!();

    let mut missing_required: Vec<String> = Vec::new();
    let mut missing_optional: Vec<String> = Vec::new();

    for dep in &sys.deps {
        if dep.path.is_some() {
            println!(
                "  {} {} ({})",
                style("✓").green().bold(),
                dep.name,
                style(dep.path.as_deref().unwrap_or("found")).dim()
            );
        } else if dep.required {
            println!(
                "  {} {} {}",
                style("✗").red().bold(),
                dep.name,
                style("(required)").red()
            );
            missing_required.push(dep.name.clone());
        } else {
            println!(
                "  {} {} {}",
                style("○").yellow().bold(),
                dep.name,
                style("(optional)").yellow()
            );
            missing_optional.push(dep.name.clone());
        }
    }

    if !missing_optional.is_empty() {
        println!(
            "\n  {} Optional: {}",
            style("Note:").yellow(),
            missing_optional.join(", ")
        );
    }

    if !missing_required.is_empty() {
        println!(
            "\n  {} Missing required: {}",
            style("Warning:").red().bold(),
            missing_required.join(", ")
        );

        let all_missing: Vec<String> = missing_required
            .iter()
            .chain(missing_optional.iter())
            .cloned()
            .collect();

        let choices = vec![
            "Install all now (single sudo)",
            "Install one at a time",
            "Skip (I'll install manually)",
        ];

        let selection = Select::new()
            .with_prompt("How would you like to install missing packages?")
            .items(&choices)
            .default(0)
            .interact()?;

        match selection {
            0 => {
                // Install all at once
                let pkg_names: Vec<&str> = all_missing.iter().map(|s| s.as_str()).collect();
                let cmd = sys.package_manager.install_cmd(&pkg_names);
                println!("\n  Running: {}", style(&cmd).dim());
                let status = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .status()?;
                if status.success() {
                    println!("  {}", style("All packages installed.").green());
                } else {
                    println!(
                        "  {} Install returned non-zero. Some packages may need manual install.",
                        style("Warning:").yellow()
                    );
                }
            }
            1 => {
                // Install one at a time
                for pkg in &all_missing {
                    let install = Confirm::new()
                        .with_prompt(format!("Install {pkg}?"))
                        .default(true)
                        .interact()?;
                    if install {
                        let cmd = sys.package_manager.install_cmd(&[pkg.as_str()]);
                        println!("  Running: {}", style(&cmd).dim());
                        let status = std::process::Command::new("sh")
                            .arg("-c")
                            .arg(&cmd)
                            .status()?;
                        if !status.success() {
                            println!("  {} Failed to install {pkg}.", style("Warning:").yellow());
                        }
                    }
                }
            }
            _ => {
                println!("  Skipping package installation.");
            }
        }
    } else {
        println!(
            "\n  {} All required dependencies are installed.",
            style("✓").green().bold()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Step 2: Subvolumes (backup sources)
// ---------------------------------------------------------------------------

fn step_subvolumes(
    sys: &SystemInfo,
    config: &mut Config,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{} {}",
        style("[2/10]").bold().cyan(),
        style("Backup Sources (BTRFS Subvolumes)").bold()
    );

    if sys.subvolumes.is_empty() {
        println!(
            "\n  {} No BTRFS subvolumes detected. Enter manually.",
            style("Note:").yellow()
        );

        loop {
            let label: String = Input::new()
                .with_prompt("Source label (e.g. nvme-root)")
                .interact_text()?;

            let volume: String = Input::new()
                .with_prompt("Top-level volume mount (e.g. /.btrfs-nvme)")
                .interact_text()?;

            let subvols_str: String = Input::new()
                .with_prompt("Subvolumes (comma-separated, e.g. @,@home)")
                .interact_text()?;
            let subvolumes: Vec<SubvolConfig> = subvols_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(|name| SubvolConfig {
                    name,
                    manual_only: false,
                    snapshot_name: None,
                })
                .collect();

            let device: String = Input::new()
                .with_prompt("Device path (e.g. /dev/nvme0n1p2)")
                .interact_text()?;

            config.sources.push(Source {
                label,
                volume,
                subvolumes,
                device,
                snapshot_dir: ".btrbk-snapshots".into(),
                target_subdirs: vec![],
                target_labels: vec![],
            });

            let add_more = Confirm::new()
                .with_prompt("Add another source?")
                .default(false)
                .interact()?;
            if !add_more {
                break;
            }
        }
    } else {
        // Filter to user-relevant subvolumes (top-level, exclude snapshots/archives)
        let subvol_names: Vec<String> = sys
            .subvolumes
            .iter()
            .filter(|s| {
                s.top_level == 5
                    && !s.name.contains(".snapshots")
                    && !s.name.contains(".archive")
                    && !s.name.starts_with("@.btrbk")
            })
            .map(|s| s.name.clone())
            .collect();

        if subvol_names.is_empty() {
            println!(
                "\n  {} No user subvolumes found at top-level.",
                style("Note:").yellow()
            );
            return Ok(());
        }

        println!(
            "\n  Detected {} top-level subvolumes:\n",
            subvol_names.len()
        );

        // Build selection items: "Select All" and "Deselect All" at top, then subvolumes
        let mut items: Vec<String> = vec![
            "── Select All ──".to_string(),
            "── Deselect All ──".to_string(),
        ];
        items.extend(subvol_names.iter().cloned());

        // Default: all subvolumes selected (indices 2..items.len())
        let defaults: Vec<bool> = items.iter().enumerate().map(|(i, _)| i >= 2).collect();

        let selected = MultiSelect::new()
            .with_prompt("Select subvolumes to back up")
            .items(&items)
            .defaults(&defaults)
            .interact()?;

        // Process selection: handle "Select All" / "Deselect All" meta-options
        let chosen_subvols: Vec<String> = if selected.contains(&0) {
            // Select All
            subvol_names.clone()
        } else if selected.contains(&1) && !selected.contains(&0) {
            // Deselect All — no subvolumes
            Vec::new()
        } else {
            // Individual selections (offset by 2 for the meta-options)
            selected
                .iter()
                .filter(|&&i| i >= 2)
                .filter_map(|&i| subvol_names.get(i - 2).cloned())
                .collect()
        };

        if chosen_subvols.is_empty() {
            println!("  {} No subvolumes selected.", style("Warning:").yellow());
        } else {
            println!("\n  Selected: {}", chosen_subvols.join(", "));

            let volume: String = Input::new()
                .with_prompt("Top-level volume mount point")
                .default("/.btrfs-root".to_string())
                .interact_text()?;

            let device: String = Input::new()
                .with_prompt("Device path for this volume")
                .interact_text()?;

            let label: String = Input::new()
                .with_prompt("Label for this source")
                .default("root".to_string())
                .interact_text()?;

            config.sources.push(Source {
                label,
                volume,
                subvolumes: chosen_subvols
                    .into_iter()
                    .map(|name| SubvolConfig {
                        name,
                        manual_only: false,
                        snapshot_name: None,
                    })
                    .collect(),
                device,
                snapshot_dir: ".btrbk-snapshots".into(),
                target_subdirs: vec![],
                target_labels: vec![],
            });
        }

        // Offer to add more sources
        while Confirm::new()
            .with_prompt("Add another source volume?")
            .default(false)
            .interact()?
        {
            let label: String = Input::new().with_prompt("Source label").interact_text()?;
            let volume: String = Input::new()
                .with_prompt("Top-level volume mount")
                .interact_text()?;
            let subvols_str: String = Input::new()
                .with_prompt("Subvolumes (comma-separated)")
                .interact_text()?;
            let subvolumes: Vec<SubvolConfig> = subvols_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .map(|name| SubvolConfig {
                    name,
                    manual_only: false,
                    snapshot_name: None,
                })
                .collect();
            let device: String = Input::new().with_prompt("Device path").interact_text()?;

            config.sources.push(Source {
                label,
                volume,
                subvolumes,
                device,
                snapshot_dir: ".btrbk-snapshots".into(),
                target_subdirs: vec![],
                target_labels: vec![],
            });
        }
    }

    println!(
        "\n  {} {} source(s) configured.",
        style("✓").green().bold(),
        config.sources.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 3: Targets (backup destinations)
// ---------------------------------------------------------------------------

fn step_targets(sys: &SystemInfo, config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{} {}",
        style("[3/10]").bold().cyan(),
        style("Backup Targets").bold()
    );

    // Show detected USB/DAS devices
    let usb_devices: Vec<&BlockDevice> = sys.devices.iter().filter(|d| d.is_usb()).collect();
    if !usb_devices.is_empty() {
        println!("\n  Detected USB/DAS devices:");
        for dev in &usb_devices {
            println!(
                "    {} {} ({}) serial={}",
                style("•").dim(),
                dev.name,
                dev.size,
                dev.serial.as_deref().unwrap_or("unknown"),
            );
        }
    } else {
        println!(
            "\n  {} No USB/DAS devices detected. Enter manually.",
            style("Note:").yellow()
        );
    }

    // Loop to add targets — default=true if none configured yet
    loop {
        let default_add = config.targets.is_empty();
        let add = Confirm::new()
            .with_prompt("Add a backup target?")
            .default(default_add)
            .interact()?;

        if !add {
            break;
        }

        let label: String = Input::new()
            .with_prompt("Target label (e.g. primary-22tb)")
            .interact_text()?;

        let serial: String = Input::new()
            .with_prompt("Drive serial number")
            .interact_text()?;

        let mount: String = Input::new()
            .with_prompt("Mount point (e.g. /mnt/backup-22tb)")
            .interact_text()?;

        let role_choices = vec!["primary", "mirror", "esp-sync"];
        let role_idx = Select::new()
            .with_prompt("Target role")
            .items(&role_choices)
            .default(0)
            .interact()?;
        let role = match role_idx {
            0 => TargetRole::Primary,
            1 => TargetRole::Mirror,
            _ => TargetRole::EspSync,
        };

        let weekly: u32 = Input::new()
            .with_prompt("Retention: weeks to keep")
            .default(4_u32)
            .interact_text()?;

        let monthly: u32 = Input::new()
            .with_prompt("Retention: months to keep")
            .default(2_u32)
            .interact_text()?;

        config.targets.push(Target {
            label,
            serial,
            mount,
            role,
            retention: Retention {
                weekly,
                monthly,
                daily: 0,
                yearly: 0,
            },
            display_name: String::new(),
        });
    }

    println!(
        "\n  {} {} target(s) configured.",
        style("✓").green().bold(),
        config.targets.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 4: ESP (EFI System Partition)
// ---------------------------------------------------------------------------

fn step_esp(sys: &SystemInfo, config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{} {}",
        style("[4/10]").bold().cyan(),
        style("EFI System Partition (ESP) Backup").bold()
    );

    // Show ESP candidates
    let esp_candidates: Vec<&BlockDevice> = sys
        .devices
        .iter()
        .filter(|d| d.is_esp_candidate())
        .collect();

    if !esp_candidates.is_empty() {
        println!("\n  Detected ESP candidates:");
        for dev in &esp_candidates {
            println!(
                "    {} /dev/{} ({} vfat)",
                style("•").dim(),
                dev.name,
                dev.size,
            );
        }
    }

    let enable_esp = Confirm::new()
        .with_prompt("Back up ESP?")
        .default(!esp_candidates.is_empty())
        .interact()?;

    if enable_esp {
        config.esp.enabled = true;

        // Auto-populate from detected candidates
        if !esp_candidates.is_empty() {
            config.esp.partitions = esp_candidates
                .iter()
                .map(|d| format!("/dev/{}", d.name))
                .collect();
            // Provide default mount points
            config.esp.mount_points = if esp_candidates.len() == 1 {
                vec!["/efi".to_string()]
            } else {
                esp_candidates
                    .iter()
                    .enumerate()
                    .map(|(i, _)| {
                        if i == 0 {
                            "/efi".to_string()
                        } else {
                            format!("/efi{}", i + 1)
                        }
                    })
                    .collect()
            };

            println!("  Auto-detected: {}", config.esp.partitions.join(", "));
        } else {
            let parts_str: String = Input::new()
                .with_prompt("ESP partitions (comma-separated, e.g. /dev/nvme0n1p1)")
                .interact_text()?;
            config.esp.partitions = parts_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let mounts_str: String = Input::new()
                .with_prompt("ESP mount points (comma-separated, e.g. /efi)")
                .default("/efi".to_string())
                .interact_text()?;
            config.esp.mount_points = mounts_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }

        // Mirror if >1 partition
        if config.esp.partitions.len() > 1 {
            config.esp.mirror = Confirm::new()
                .with_prompt("Enable ESP mirroring between partitions?")
                .default(true)
                .interact()?;
        }

        // Hooks — auto-detect type from package manager
        let auto_hook_type = match &sys.package_manager {
            PackageManager::Pacman => HookType::Pacman,
            PackageManager::Apt => HookType::Apt,
            PackageManager::Dnf => HookType::Dnf,
            _ => HookType::None,
        };

        if auto_hook_type != HookType::None {
            let hook_label = match auto_hook_type {
                HookType::Pacman => "pacman",
                HookType::Apt => "apt",
                HookType::Dnf => "dnf",
                HookType::None => "none",
            };
            let enable_hooks = Confirm::new()
                .with_prompt(format!(
                    "Install {hook_label} hook for automatic ESP sync after kernel updates?"
                ))
                .default(true)
                .interact()?;
            if enable_hooks {
                config.esp.hooks.enabled = true;
                config.esp.hooks.hook_type = auto_hook_type;
            }
        }
    } else {
        config.esp.enabled = false;
    }

    println!(
        "\n  {} ESP: {}",
        style("✓").green().bold(),
        if config.esp.enabled {
            let parts = config.esp.partitions.join(", ");
            format!(
                "enabled ({}{})",
                parts,
                if config.esp.mirror { " + mirror" } else { "" }
            )
        } else {
            "disabled".to_string()
        }
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 5: Retention
// ---------------------------------------------------------------------------

fn step_retention(config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{} {}",
        style("[5/10]").bold().cyan(),
        style("Retention Policy").bold()
    );

    if config.targets.is_empty() {
        println!(
            "  {} No targets configured — skipping retention.",
            style("Note:").yellow()
        );
        return Ok(());
    }

    println!("\n  Current retention defaults:");
    for target in &config.targets {
        println!(
            "    {} {}: {} weeks, {} months",
            style("•").dim(),
            target.label,
            target.retention.weekly,
            target.retention.monthly,
        );
    }

    let customize = Confirm::new()
        .with_prompt("Customize retention per target?")
        .default(false)
        .interact()?;

    if customize {
        for target in &mut config.targets {
            println!("\n  Target: {}", style(&target.label).bold());
            target.retention.weekly = Input::new()
                .with_prompt("  Weeks to keep")
                .default(target.retention.weekly)
                .interact_text()?;
            target.retention.monthly = Input::new()
                .with_prompt("  Months to keep")
                .default(target.retention.monthly)
                .interact_text()?;
        }
    }

    println!("\n  {} Retention configured.", style("✓").green().bold());
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 6: Scheduling
// ---------------------------------------------------------------------------

fn step_scheduling(
    sys: &SystemInfo,
    config: &mut Config,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{} {}",
        style("[6/10]").bold().cyan(),
        style("Backup Schedule").bold()
    );

    // Set init system from detection
    config.init.system = match sys.init_system {
        InitSystemDetected::Systemd => InitSystem::Systemd,
        InitSystemDetected::Openrc => InitSystem::Openrc,
        InitSystemDetected::Sysvinit => InitSystem::Sysvinit,
    };

    let init_label = match config.init.system {
        InitSystem::Systemd => "systemd (timers)",
        InitSystem::Openrc => "OpenRC (cron)",
        InitSystem::Sysvinit => "SysVinit (cron)",
    };

    println!("\n  Init system: {}", style(init_label).bold());
    println!(
        "  Incremental: {} daily",
        style(&config.schedule.incremental).bold()
    );
    println!(
        "  Full:        {} weekly",
        style(&config.schedule.full).bold()
    );

    let customize = Confirm::new()
        .with_prompt("Customize schedule?")
        .default(false)
        .interact()?;

    if customize {
        config.schedule.incremental = Input::new()
            .with_prompt("Incremental backup time (HH:MM)")
            .default(config.schedule.incremental.clone())
            .interact_text()?;

        config.schedule.full = Input::new()
            .with_prompt("Full backup schedule (e.g. Sun 04:00)")
            .default(config.schedule.full.clone())
            .interact_text()?;

        config.schedule.randomized_delay_min = Input::new()
            .with_prompt("Randomized delay (minutes)")
            .default(config.schedule.randomized_delay_min)
            .interact_text()?;
    }

    println!(
        "\n  {} Schedule configured ({}).",
        style("✓").green().bold(),
        init_label,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 7: Email
// ---------------------------------------------------------------------------

fn step_email(config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{} {}",
        style("[7/10]").bold().cyan(),
        style("Email Notifications").bold()
    );

    let enable = Confirm::new()
        .with_prompt("Enable email reports after backup?")
        .default(config.email.enabled)
        .interact()?;

    if enable {
        config.email.enabled = true;

        config.email.smtp_host = Input::new()
            .with_prompt("SMTP host")
            .default(if config.email.smtp_host.is_empty() {
                "localhost".to_string()
            } else {
                config.email.smtp_host.clone()
            })
            .interact_text()?;

        config.email.smtp_port = Input::new()
            .with_prompt("SMTP port")
            .default(if config.email.smtp_port == 0 {
                587_u16
            } else {
                config.email.smtp_port
            })
            .interact_text()?;

        config.email.from = Input::new()
            .with_prompt("From address")
            .default(if config.email.from.is_empty() {
                "backup@localhost".to_string()
            } else {
                config.email.from.clone()
            })
            .interact_text()?;

        config.email.to = Input::new()
            .with_prompt("To address")
            .default(if config.email.to.is_empty() {
                "root@localhost".to_string()
            } else {
                config.email.to.clone()
            })
            .interact_text()?;

        let auth_choices = vec!["none", "plain", "starttls"];
        let auth_default = match config.email.auth {
            AuthMethod::None => 0,
            AuthMethod::Plain => 1,
            AuthMethod::Starttls => 2,
        };
        let auth_idx = Select::new()
            .with_prompt("Authentication method")
            .items(&auth_choices)
            .default(auth_default)
            .interact()?;
        config.email.auth = match auth_idx {
            1 => AuthMethod::Plain,
            2 => AuthMethod::Starttls,
            _ => AuthMethod::None,
        };
    } else {
        config.email.enabled = false;
    }

    println!(
        "\n  {} Email: {}",
        style("✓").green().bold(),
        if config.email.enabled {
            format!("enabled ({})", config.email.smtp_host)
        } else {
            "disabled".to_string()
        }
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 8: Install location
// ---------------------------------------------------------------------------

fn step_install_location(config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{} {}",
        style("[8/10]").bold().cyan(),
        style("Install Location").bold()
    );

    println!(
        "\n  Prefix:  {}",
        style(&config.general.install_prefix).bold()
    );
    println!("  DB path: {}", style(&config.general.db_path).bold());

    let customize = Confirm::new()
        .with_prompt("Customize install paths?")
        .default(false)
        .interact()?;

    if customize {
        config.general.install_prefix = Input::new()
            .with_prompt("Install prefix")
            .default(config.general.install_prefix.clone())
            .interact_text()?;

        config.general.db_path = Input::new()
            .with_prompt("Database path")
            .default(config.general.db_path.clone())
            .interact_text()?;
    }

    println!(
        "\n  {} Install paths configured.",
        style("✓").green().bold()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 9: GUI
// ---------------------------------------------------------------------------

fn step_gui(config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{} {}",
        style("[9/10]").bold().cyan(),
        style("KDE Plasma GUI").bold()
    );

    config.gui.enabled = Confirm::new()
        .with_prompt("Install KDE Plasma GUI? (requires Qt6/KF6)")
        .default(false)
        .interact()?;

    println!(
        "\n  {} GUI: {}",
        style("✓").green().bold(),
        if config.gui.enabled {
            "will be installed"
        } else {
            "skipped"
        }
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 10: Review and confirm
// ---------------------------------------------------------------------------

fn step_review(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\n{} {}",
        style("[10/10]").bold().cyan(),
        style("Review Configuration").bold()
    );
    println!("\n{}", style("═".repeat(50)).dim());

    // Sources
    println!(
        "\n  {} ({}):",
        style("Sources").bold(),
        config.sources.len()
    );
    for src in &config.sources {
        println!("    {} {} [{}]", style("•").dim(), src.label, src.device);
        println!(
            "      volume: {}, subvols: {}",
            src.volume,
            src.subvolumes
                .iter()
                .map(|sv| sv.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Targets
    println!(
        "\n  {} ({}):",
        style("Targets").bold(),
        config.targets.len()
    );
    for tgt in &config.targets {
        let role_str = match tgt.role {
            TargetRole::Primary => "primary",
            TargetRole::Mirror => "mirror",
            TargetRole::EspSync => "esp-sync",
        };
        println!(
            "    {} {} [{}] role={}",
            style("•").dim(),
            tgt.label,
            tgt.serial,
            role_str,
        );
        println!(
            "      mount: {}, retention: {}w {}m",
            tgt.mount, tgt.retention.weekly, tgt.retention.monthly
        );
    }

    // ESP
    println!("\n  {}:", style("ESP").bold());
    if config.esp.enabled {
        println!("    partitions: {}", config.esp.partitions.join(", "));
        println!("    mirror: {}", config.esp.mirror);
        if config.esp.hooks.enabled {
            let hook_str = match config.esp.hooks.hook_type {
                HookType::Pacman => "pacman",
                HookType::Apt => "apt",
                HookType::Dnf => "dnf",
                HookType::None => "none",
            };
            println!("    hooks: {hook_str}");
        }
    } else {
        println!("    disabled");
    }

    // Schedule
    println!("\n  {}:", style("Schedule").bold());
    let init_str = match config.init.system {
        InitSystem::Systemd => "systemd",
        InitSystem::Openrc => "openrc",
        InitSystem::Sysvinit => "sysvinit",
    };
    println!("    init: {init_str}");
    println!("    incremental: {}", config.schedule.incremental);
    println!("    full: {}", config.schedule.full);

    // Email
    println!("\n  {}:", style("Email").bold());
    if config.email.enabled {
        println!(
            "    {}:{} from={} to={}",
            config.email.smtp_host, config.email.smtp_port, config.email.from, config.email.to
        );
    } else {
        println!("    disabled");
    }

    // Install
    println!("\n  {}:", style("Install").bold());
    println!("    prefix: {}", config.general.install_prefix);
    println!("    db: {}", config.general.db_path);

    // GUI
    println!("\n  {}:", style("GUI").bold());
    println!(
        "    {}",
        if config.gui.enabled {
            "will be installed"
        } else {
            "not installed"
        }
    );

    println!("\n{}", style("═".repeat(50)).dim());

    // Validate
    let warnings = config.validate();
    if !warnings.is_empty() {
        println!("\n  {} Validation warnings:", style("⚠").yellow().bold());
        for w in &warnings {
            println!("    {} {w}", style("•").yellow());
        }
        println!();
    }

    // Final confirmation
    let proceed = Confirm::new()
        .with_prompt("Proceed with installation?")
        .default(true)
        .interact()?;

    if !proceed {
        return Err("Setup cancelled by user.".into());
    }

    println!("\n  {} Configuration accepted.", style("✓").green().bold());
    Ok(())
}
