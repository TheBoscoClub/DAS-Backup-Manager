// Interactive setup wizard using dialoguer prompts and console styling.
//
// The step count lives in `TOTAL_STEPS` rather than being written into each
// banner: the numbering had drifted to "[3/10]" followed by "[5/10]" across nine
// actual steps, so the wizard skipped a number and overstated its own length
// (bd DAS-Backup-Manager-kvg).
//
// Binary-only module (main.rs scope). Every step talks to the operator through
// the private `Prompter` trait instead of calling dialoguer directly, so the
// decisions a step makes — which subvolumes become sources, which role and
// serial a target gets, which defaults are offered — are driven by a scripted
// session in the tests below. `Terminal` is the only implementation that needs
// a TTY.

use std::io::Write;
use std::str::FromStr;

use console::style;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use crate::setup::config::*;
use crate::setup::detect::*;

/// Everything the wizard does to the outside world: print a line, ask a
/// question, run an install command.
///
/// The steps hold the control flow and the wording; an implementation only
/// decides how a question reaches the operator and how the answer comes back.
trait Prompter {
    /// Print one line (a trailing newline is added).
    fn say(&mut self, text: &str);
    /// Yes/no question with a preselected answer.
    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool, Box<dyn std::error::Error>>;
    /// Pick exactly one of `items`; returns its index.
    fn select(
        &mut self,
        prompt: &str,
        items: &[&str],
        default: usize,
    ) -> Result<usize, Box<dyn std::error::Error>>;
    /// Pick any number of `items`, `defaults[i]` being whether item `i` starts
    /// checked; returns the checked indices.
    fn multi_select(
        &mut self,
        prompt: &str,
        items: &[String],
        defaults: &[bool],
    ) -> Result<Vec<usize>, Box<dyn std::error::Error>>;
    /// Free-text answer parsed as `T`. With `Some(default)`, an empty answer
    /// takes the default.
    fn input<T>(
        &mut self,
        prompt: &str,
        default: Option<T>,
    ) -> Result<T, Box<dyn std::error::Error>>
    where
        T: Clone + ToString + FromStr,
        <T as FromStr>::Err: ToString;
    /// Run `cmd` through `sh -c`; `Ok(true)` when it exited zero.
    fn run_shell(&mut self, cmd: &str) -> Result<bool, Box<dyn std::error::Error>>;
}

/// The production `Prompter`: dialoguer prompts on the controlling terminal,
/// lines written to `out` (stdout outside the tests).
struct Terminal<W: Write> {
    out: W,
}

impl<W: Write> Prompter for Terminal<W> {
    fn say(&mut self, text: &str) {
        // Same contract as `println!`: a wizard that cannot print cannot be
        // answered, so there is nothing sensible to continue with.
        writeln!(self.out, "{text}").expect("failed printing wizard output");
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(Confirm::new()
            .with_prompt(prompt)
            .default(default)
            .interact()?)
    }

    fn select(
        &mut self,
        prompt: &str,
        items: &[&str],
        default: usize,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        Ok(Select::new()
            .with_prompt(prompt)
            .items(items)
            .default(default)
            .interact()?)
    }

    fn multi_select(
        &mut self,
        prompt: &str,
        items: &[String],
        defaults: &[bool],
    ) -> Result<Vec<usize>, Box<dyn std::error::Error>> {
        Ok(MultiSelect::new()
            .with_prompt(prompt)
            .items(items)
            .defaults(defaults)
            .interact()?)
    }

    fn input<T>(
        &mut self,
        prompt: &str,
        default: Option<T>,
    ) -> Result<T, Box<dyn std::error::Error>>
    where
        T: Clone + ToString + FromStr,
        <T as FromStr>::Err: ToString,
    {
        let input = Input::<T>::new().with_prompt(prompt);
        let input = match default {
            Some(value) => input.default(value),
            None => input,
        };
        Ok(input.interact_text()?)
    }

    fn run_shell(&mut self, cmd: &str) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .status()?
            .success())
    }
}

/// `println!` for a `Prompter`.
macro_rules! say {
    ($p:expr) => {
        $p.say("")
    };
    ($p:expr, $($arg:tt)*) => {
        $p.say(&format!($($arg)*))
    };
}

/// Number of interactive steps the wizard walks through.
///
/// Single source for every banner's denominator, so adding or removing a step
/// cannot leave the printed count stale.
const TOTAL_STEPS: usize = 9;

/// Run the interactive setup wizard. Takes detected system info and an optional
/// existing config (for --modify mode). Returns a completed, validated Config.
pub fn run_wizard(
    sys: &SystemInfo,
    existing: Option<Config>,
) -> Result<Config, Box<dyn std::error::Error>> {
    let mut terminal = Terminal {
        out: std::io::stdout(),
    };
    run_wizard_with(&mut terminal, sys, existing)
}

fn run_wizard_with(
    p: &mut impl Prompter,
    sys: &SystemInfo,
    existing: Option<Config>,
) -> Result<Config, Box<dyn std::error::Error>> {
    let mut config = existing.unwrap_or_default();

    say!(p, "\n{}", style("ButteredDASD Setup Wizard").bold().cyan());
    say!(p, "{}\n", style("─".repeat(40)).dim());

    step_dependencies(p, sys)?;
    step_subvolumes(p, sys, &mut config)?;
    step_targets(p, sys, &mut config)?;
    step_retention(p, &mut config)?;
    step_scheduling(p, sys, &mut config)?;
    step_email(p, &mut config)?;
    step_install_location(p, &mut config)?;
    step_gui(p, &mut config)?;
    step_review(p, &config)?;

    Ok(config)
}

// ---------------------------------------------------------------------------
// Step 1: Dependencies
// ---------------------------------------------------------------------------

fn step_dependencies(
    p: &mut impl Prompter,
    sys: &SystemInfo,
) -> Result<(), Box<dyn std::error::Error>> {
    say!(
        p,
        "\n{} {}",
        style(format!("[1/{TOTAL_STEPS}]")).bold().cyan(),
        style("Checking Dependencies").bold()
    );
    say!(p);

    let mut missing_required: Vec<String> = Vec::new();
    let mut missing_optional: Vec<String> = Vec::new();

    for dep in &sys.deps {
        if dep.path.is_some() {
            say!(
                p,
                "  {} {} ({})",
                style("✓").green().bold(),
                dep.name,
                style(dep.path.as_deref().unwrap_or("found")).dim()
            );
        } else if dep.required {
            say!(
                p,
                "  {} {} {}",
                style("✗").red().bold(),
                dep.name,
                style("(required)").red()
            );
            missing_required.push(dep.name.clone());
        } else {
            say!(
                p,
                "  {} {} {}",
                style("○").yellow().bold(),
                dep.name,
                style("(optional)").yellow()
            );
            missing_optional.push(dep.name.clone());
        }
    }

    if !missing_optional.is_empty() {
        say!(
            p,
            "\n  {} Optional: {}",
            style("Note:").yellow(),
            missing_optional.join(", ")
        );
    }

    if !missing_required.is_empty() {
        say!(
            p,
            "\n  {} Missing required: {}",
            style("Warning:").red().bold(),
            missing_required.join(", ")
        );

        let all_missing: Vec<String> = missing_required
            .iter()
            .chain(missing_optional.iter())
            .cloned()
            .collect();

        let choices = [
            "Install all now (single sudo)",
            "Install one at a time",
            "Skip (I'll install manually)",
        ];

        let selection = p.select(
            "How would you like to install missing packages?",
            &choices,
            0,
        )?;

        match selection {
            0 => {
                // Install all at once
                let pkg_names: Vec<&str> = all_missing.iter().map(|s| s.as_str()).collect();
                let cmd = sys.package_manager.install_cmd(&pkg_names);
                say!(p, "\n  Running: {}", style(&cmd).dim());
                if p.run_shell(&cmd)? {
                    say!(p, "  {}", style("All packages installed.").green());
                } else {
                    say!(
                        p,
                        "  {} Install returned non-zero. Some packages may need manual install.",
                        style("Warning:").yellow()
                    );
                }
            }
            1 => {
                // Install one at a time
                for pkg in &all_missing {
                    if p.confirm(&format!("Install {pkg}?"), true)? {
                        let cmd = sys.package_manager.install_cmd(&[pkg.as_str()]);
                        say!(p, "  Running: {}", style(&cmd).dim());
                        if !p.run_shell(&cmd)? {
                            say!(
                                p,
                                "  {} Failed to install {pkg}.",
                                style("Warning:").yellow()
                            );
                        }
                    }
                }
            }
            _ => {
                say!(p, "  Skipping package installation.");
            }
        }
    } else {
        say!(
            p,
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
    p: &mut impl Prompter,
    sys: &SystemInfo,
    config: &mut Config,
) -> Result<(), Box<dyn std::error::Error>> {
    say!(
        p,
        "\n{} {}",
        style(format!("[2/{TOTAL_STEPS}]")).bold().cyan(),
        style("Backup Sources (BTRFS Subvolumes)").bold()
    );

    if sys.subvolumes.is_empty() {
        say!(
            p,
            "\n  {} No BTRFS subvolumes detected. Enter manually.",
            style("Note:").yellow()
        );

        loop {
            let label: String = p.input("Source label (e.g. nvme-root)", None)?;

            let volume: String = p.input("Top-level volume mount (e.g. /.btrfs-nvme)", None)?;

            let subvols_str: String =
                p.input("Subvolumes (comma-separated, e.g. @,@home)", None)?;
            let subvolumes = parse_subvol_list(&subvols_str);

            let device: String = p.input("Device path (e.g. /dev/nvme0n1p2)", None)?;

            config.sources.push(Source {
                label,
                volume,
                subvolumes,
                device,
                snapshot_dir: ".btrbk-snapshots".into(),
                target_subdirs: vec![],
                target_labels: vec![],
            });

            let add_more = p.confirm("Add another source?", false)?;
            if !add_more {
                break;
            }
        }
    } else {
        let subvol_names = backup_candidates(&sys.subvolumes);

        if subvol_names.is_empty() {
            say!(
                p,
                "\n  {} No user subvolumes found at top-level.",
                style("Note:").yellow()
            );
            return Ok(());
        }

        say!(
            p,
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

        let selected = p.multi_select("Select subvolumes to back up", &items, &defaults)?;

        let chosen_subvols = resolve_selection(&selected, &subvol_names);

        if chosen_subvols.is_empty() {
            say!(
                p,
                "  {} No subvolumes selected.",
                style("Warning:").yellow()
            );
        } else {
            say!(p, "\n  Selected: {}", chosen_subvols.join(", "));

            let volume: String = p.input(
                "Top-level volume mount point",
                Some("/.btrfs-root".to_string()),
            )?;

            let device: String = p.input("Device path for this volume", None)?;

            let label: String = p.input("Label for this source", Some("root".to_string()))?;

            config.sources.push(Source {
                label,
                volume,
                subvolumes: chosen_subvols
                    .into_iter()
                    .map(|name| SubvolConfig {
                        name,
                        ..Default::default()
                    })
                    .collect(),
                device,
                snapshot_dir: ".btrbk-snapshots".into(),
                target_subdirs: vec![],
                target_labels: vec![],
            });
        }

        // Offer to add more sources
        while p.confirm("Add another source volume?", false)? {
            let label: String = p.input("Source label", None)?;
            let volume: String = p.input("Top-level volume mount", None)?;
            let subvols_str: String = p.input("Subvolumes (comma-separated)", None)?;
            let subvolumes = parse_subvol_list(&subvols_str);
            let device: String = p.input("Device path", None)?;

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

    say!(
        p,
        "\n  {} {} source(s) configured.",
        style("✓").green().bold(),
        config.sources.len()
    );
    Ok(())
}

/// Detected subvolumes worth offering as backup sources: direct children of
/// the filesystem root (BTRFS id 5), minus snapshot and archive trees and
/// btrbk's own snapshot directories — backing those up would back up backups.
fn backup_candidates(subvolumes: &[SubvolumeInfo]) -> Vec<String> {
    subvolumes
        .iter()
        .filter(|s| {
            s.top_level == 5
                && !s.name.contains(".snapshots")
                && !s.name.contains(".archive")
                && !s.name.starts_with("@.btrbk")
        })
        .map(|s| s.name.clone())
        .collect()
}

/// Turn the checked rows of the subvolume list into subvolume names.
///
/// Rows 0 and 1 are the "Select All" / "Deselect All" meta-options, so
/// subvolume `n` sits at row `n + 2`. "Select All" wins over everything else
/// that is checked; "Deselect All" wins over individually checked rows.
fn resolve_selection(selected: &[usize], subvol_names: &[String]) -> Vec<String> {
    if selected.contains(&0) {
        subvol_names.to_vec()
    } else if selected.contains(&1) {
        Vec::new()
    } else {
        selected
            .iter()
            .filter_map(|&i| subvol_names.get(i.checked_sub(2)?).cloned())
            .collect()
    }
}

/// Parse a comma-separated subvolume list as typed by the operator, ignoring
/// surrounding whitespace and empty entries (`"@, @home,"` is two subvolumes).
fn parse_subvol_list(input: &str) -> Vec<SubvolConfig> {
    input
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|name| SubvolConfig {
            name,
            ..Default::default()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Step 3: Targets (backup destinations)
// ---------------------------------------------------------------------------

fn step_targets(
    p: &mut impl Prompter,
    sys: &SystemInfo,
    config: &mut Config,
) -> Result<(), Box<dyn std::error::Error>> {
    say!(
        p,
        "\n{} {}",
        style(format!("[3/{TOTAL_STEPS}]")).bold().cyan(),
        style("Backup Targets").bold()
    );

    // Show detected USB/DAS devices
    let usb_devices: Vec<&BlockDevice> = sys.devices.iter().filter(|d| d.is_usb()).collect();
    if !usb_devices.is_empty() {
        say!(p, "\n  Detected USB/DAS devices:");
        for dev in &usb_devices {
            say!(
                p,
                "    {} {} ({}) serial={}",
                style("•").dim(),
                dev.name,
                dev.size,
                dev.serial.as_deref().unwrap_or("unknown"),
            );
        }
    } else {
        say!(
            p,
            "\n  {} No USB/DAS devices detected. Enter manually.",
            style("Note:").yellow()
        );
    }

    // Loop to add targets — default=true if none configured yet
    loop {
        let default_add = config.targets.is_empty();
        let add = p.confirm("Add a backup target?", default_add)?;

        if !add {
            break;
        }

        let label: String = p.input("Target label (e.g. primary-22tb)", None)?;

        let serial: String = p.input("Drive serial number", None)?;

        let mount: String = p.input("Mount point (e.g. /mnt/backup-22tb)", None)?;

        let role_choices = ["primary", "mirror"];
        let role_idx = p.select("Target role", &role_choices, 0)?;
        let role = match role_idx {
            0 => TargetRole::Primary,
            _ => TargetRole::Mirror,
        };

        let weekly: u32 = p.input("Retention: weeks to keep", Some(4_u32))?;

        let monthly: u32 = p.input("Retention: months to keep", Some(2_u32))?;

        config.targets.push(Target {
            label,
            serials: vec![serial.clone()],
            serial,
            mount_uuid: None,
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

    say!(
        p,
        "\n  {} {} target(s) configured.",
        style("✓").green().bold(),
        config.targets.len()
    );

    // Policy: audiobook source content must NEVER land on the 2TB recovery drives.
    // The recovery drives are bootable standalone systems with limited capacity;
    // ~509 GiB of audiobook source files would overflow them and defeat the recovery role.
    // Restrict any source whose label contains "audiobook" (case-insensitive) to the
    // primary target(s) only. Skips sources whose target_labels are already non-empty
    // (the user has made an explicit choice).
    let primary_labels: Vec<String> = config
        .targets
        .iter()
        .filter(|t| t.role == TargetRole::Primary)
        .map(|t| t.label.clone())
        .collect();
    if !primary_labels.is_empty() {
        for source in config.sources.iter_mut() {
            if source.target_labels.is_empty() && source.label.to_lowercase().contains("audiobook")
            {
                source.target_labels = primary_labels.clone();
                say!(
                    p,
                    "  {} Source '{}' restricted to primary target(s) only: {}",
                    style("→").dim(),
                    source.label,
                    primary_labels.join(", ")
                );
            }
        }
    }

    Ok(())
}

// step_esp() removed 2026-04-12 — see .claude/rules/esp-safety.md.

// ---------------------------------------------------------------------------
// Step 4: Retention
// ---------------------------------------------------------------------------

fn step_retention(
    p: &mut impl Prompter,
    config: &mut Config,
) -> Result<(), Box<dyn std::error::Error>> {
    say!(
        p,
        "\n{} {}",
        style(format!("[4/{TOTAL_STEPS}]")).bold().cyan(),
        style("Retention Policy").bold()
    );

    if config.targets.is_empty() {
        say!(
            p,
            "  {} No targets configured — skipping retention.",
            style("Note:").yellow()
        );
        return Ok(());
    }

    say!(p, "\n  Current retention defaults:");
    for target in &config.targets {
        say!(
            p,
            "    {} {}: {} weeks, {} months",
            style("•").dim(),
            target.label,
            target.retention.weekly,
            target.retention.monthly,
        );
    }

    let customize = p.confirm("Customize retention per target?", false)?;

    if customize {
        for target in &mut config.targets {
            say!(p, "\n  Target: {}", style(&target.label).bold());
            target.retention.weekly = p.input("  Weeks to keep", Some(target.retention.weekly))?;
            target.retention.monthly =
                p.input("  Months to keep", Some(target.retention.monthly))?;
        }
    }

    say!(p, "\n  {} Retention configured.", style("✓").green().bold());
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 5: Scheduling
// ---------------------------------------------------------------------------

fn step_scheduling(
    p: &mut impl Prompter,
    sys: &SystemInfo,
    config: &mut Config,
) -> Result<(), Box<dyn std::error::Error>> {
    say!(
        p,
        "\n{} {}",
        style(format!("[5/{TOTAL_STEPS}]")).bold().cyan(),
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

    say!(p, "\n  Init system: {}", style(init_label).bold());
    say!(
        p,
        "  Incremental: {} daily",
        style(&config.schedule.incremental).bold()
    );
    say!(
        p,
        "  Full:        {} weekly",
        style(&config.schedule.full).bold()
    );

    let customize = p.confirm("Customize schedule?", false)?;

    if customize {
        config.schedule.incremental = p.input(
            "Incremental backup time (HH:MM)",
            Some(config.schedule.incremental.clone()),
        )?;

        config.schedule.full = p.input(
            "Full backup schedule (e.g. Sun 04:00)",
            Some(config.schedule.full.clone()),
        )?;

        config.schedule.randomized_delay_min = p.input(
            "Randomized delay (minutes)",
            Some(config.schedule.randomized_delay_min),
        )?;
    }

    say!(
        p,
        "\n  {} Schedule configured ({}).",
        style("✓").green().bold(),
        init_label,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 6: Email
// ---------------------------------------------------------------------------

fn step_email(
    p: &mut impl Prompter,
    config: &mut Config,
) -> Result<(), Box<dyn std::error::Error>> {
    say!(
        p,
        "\n{} {}",
        style(format!("[6/{TOTAL_STEPS}]")).bold().cyan(),
        style("Email Notifications").bold()
    );

    let enable = p.confirm("Enable email reports after backup?", config.email.enabled)?;

    if enable {
        config.email.enabled = true;

        say!(
            p,
            "  Reports are submitted unauthenticated to a local mail relay,\n  \
             which holds the upstream credential. btrdasd never stores one."
        );

        config.email.smtp_host = p.input(
            "Relay host",
            Some(if config.email.smtp_host.is_empty() {
                "127.0.0.1".to_string()
            } else {
                config.email.smtp_host.clone()
            }),
        )?;

        config.email.smtp_port = p.input(
            "Relay port",
            Some(if config.email.smtp_port == 0 {
                25_u16
            } else {
                config.email.smtp_port
            }),
        )?;

        // The relay routes by envelope sender: this address selects which
        // upstream credential the relay authenticates with. It must be an
        // address the relay is configured for, or the relay's canonical-sender
        // fallback rewrites it and reports arrive under another identity.
        config.email.from = p.input(
            "From address (selects the relay's upstream credential)",
            Some(if config.email.from.is_empty() {
                "backup@localhost".to_string()
            } else {
                config.email.from.clone()
            }),
        )?;

        config.email.to = p.input(
            "To address",
            Some(if config.email.to.is_empty() {
                "root@localhost".to_string()
            } else {
                config.email.to.clone()
            }),
        )?;
    } else {
        config.email.enabled = false;
    }

    say!(
        p,
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
// Step 7: Install location
// ---------------------------------------------------------------------------

fn step_install_location(
    p: &mut impl Prompter,
    config: &mut Config,
) -> Result<(), Box<dyn std::error::Error>> {
    say!(
        p,
        "\n{} {}",
        style(format!("[7/{TOTAL_STEPS}]")).bold().cyan(),
        style("Install Location").bold()
    );

    say!(
        p,
        "\n  Prefix:  {}",
        style(&config.general.install_prefix).bold()
    );
    say!(p, "  DB path: {}", style(&config.general.db_path).bold());

    let customize = p.confirm("Customize install paths?", false)?;

    if customize {
        config.general.install_prefix = p.input(
            "Install prefix",
            Some(config.general.install_prefix.clone()),
        )?;

        config.general.db_path = p.input("Database path", Some(config.general.db_path.clone()))?;
    }

    say!(
        p,
        "\n  {} Install paths configured.",
        style("✓").green().bold()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Step 8: GUI
// ---------------------------------------------------------------------------

fn step_gui(p: &mut impl Prompter, config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    say!(
        p,
        "\n{} {}",
        style(format!("[8/{TOTAL_STEPS}]")).bold().cyan(),
        style("KDE Plasma GUI").bold()
    );

    config.gui.enabled = p.confirm("Install KDE Plasma GUI? (requires Qt6/KF6)", false)?;

    say!(
        p,
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
// Step 9: Review and confirm
// ---------------------------------------------------------------------------

fn step_review(p: &mut impl Prompter, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    say!(
        p,
        "\n{} {}",
        style(format!("[9/{TOTAL_STEPS}]")).bold().cyan(),
        style("Review Configuration").bold()
    );
    say!(p, "\n{}", style("═".repeat(50)).dim());

    // Sources
    say!(
        p,
        "\n  {} ({}):",
        style("Sources").bold(),
        config.sources.len()
    );
    for src in &config.sources {
        say!(p, "    {} {} [{}]", style("•").dim(), src.label, src.device);
        say!(
            p,
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
    say!(
        p,
        "\n  {} ({}):",
        style("Targets").bold(),
        config.targets.len()
    );
    for tgt in &config.targets {
        let role_str = match tgt.role {
            TargetRole::Primary => "primary",
            TargetRole::Mirror => "mirror",
        };
        say!(
            p,
            "    {} {} [{}] role={}",
            style("•").dim(),
            tgt.label,
            tgt.serial,
            role_str,
        );
        say!(
            p,
            "      mount: {}, retention: {}w {}m",
            tgt.mount,
            tgt.retention.weekly,
            tgt.retention.monthly
        );
    }

    // Schedule
    say!(p, "\n  {}:", style("Schedule").bold());
    let init_str = match config.init.system {
        InitSystem::Systemd => "systemd",
        InitSystem::Openrc => "openrc",
        InitSystem::Sysvinit => "sysvinit",
    };
    say!(p, "    init: {init_str}");
    say!(p, "    incremental: {}", config.schedule.incremental);
    say!(p, "    full: {}", config.schedule.full);

    // Email
    say!(p, "\n  {}:", style("Email").bold());
    if config.email.enabled {
        say!(
            p,
            "    {}:{} from={} to={}",
            config.email.smtp_host,
            config.email.smtp_port,
            config.email.from,
            config.email.to
        );
    } else {
        say!(p, "    disabled");
    }

    // Install
    say!(p, "\n  {}:", style("Install").bold());
    say!(p, "    prefix: {}", config.general.install_prefix);
    say!(p, "    db: {}", config.general.db_path);

    // GUI
    say!(p, "\n  {}:", style("GUI").bold());
    say!(
        p,
        "    {}",
        if config.gui.enabled {
            "will be installed"
        } else {
            "not installed"
        }
    );

    say!(p, "\n{}", style("═".repeat(50)).dim());

    // Validate
    let warnings = config.validate();
    if !warnings.is_empty() {
        say!(p, "\n  {} Validation warnings:", style("⚠").yellow().bold());
        for w in &warnings {
            say!(p, "    {} {w}", style("•").yellow());
        }
        say!(p);
    }

    // Final confirmation
    let proceed = p.confirm("Proceed with installation?", true)?;

    if !proceed {
        return Err("Setup cancelled by user.".into());
    }

    say!(
        p,
        "\n  {} Configuration accepted.",
        style("✓").green().bold()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::process::{Command, Stdio};

    // -----------------------------------------------------------------------
    // Scripted operator
    // -----------------------------------------------------------------------

    /// One operator action, consumed in order. A reply of the wrong kind for
    /// the question being asked fails the test, so a script also pins the
    /// order of the questions.
    enum Reply {
        /// Press Enter: take whatever the prompt offers as its default.
        Accept,
        Confirm(bool),
        Select(usize),
        Multi(Vec<usize>),
        Text(&'static str),
        /// Outcome of the next install command (`true` = exit 0).
        Shell(bool),
    }

    /// A `Prompter` that answers from a script and records everything the
    /// wizard printed, asked and ran.
    struct Script {
        replies: VecDeque<Reply>,
        said: Vec<String>,
        asked: Vec<String>,
        ran: Vec<String>,
    }

    impl Script {
        fn new(replies: Vec<Reply>) -> Self {
            Self {
                replies: replies.into(),
                said: Vec::new(),
                asked: Vec::new(),
                ran: Vec::new(),
            }
        }

        fn next(&mut self, question: &str) -> Reply {
            self.replies
                .pop_front()
                .unwrap_or_else(|| panic!("script ran out of replies at: {question}"))
        }

        /// Everything printed, colour codes removed, one line per `say`.
        fn output(&self) -> String {
            self.said.join("\n")
        }

        fn assert_said(&self, fragment: &str) {
            let output = self.output();
            assert!(
                output.contains(fragment),
                "expected {fragment:?} in wizard output:\n{output}"
            );
        }

        fn assert_not_said(&self, fragment: &str) {
            let output = self.output();
            assert!(
                !output.contains(fragment),
                "did not expect {fragment:?} in wizard output:\n{output}"
            );
        }

        /// The session must have used every scripted reply: a leftover means a
        /// question the wizard was expected to ask was never asked.
        fn assert_finished(&self) {
            assert!(
                self.replies.is_empty(),
                "{} scripted replies were never asked for; questions asked: {:#?}",
                self.replies.len(),
                self.asked
            );
        }
    }

    impl Prompter for Script {
        fn say(&mut self, text: &str) {
            self.said.push(console::strip_ansi_codes(text).into_owned());
        }

        fn confirm(
            &mut self,
            prompt: &str,
            default: bool,
        ) -> Result<bool, Box<dyn std::error::Error>> {
            let question = format!("confirm: {prompt} [{default}]");
            let reply = self.next(&question);
            self.asked.push(question.clone());
            Ok(match reply {
                Reply::Accept => default,
                Reply::Confirm(answer) => answer,
                _ => panic!("script has the wrong kind of reply for: {question}"),
            })
        }

        fn select(
            &mut self,
            prompt: &str,
            items: &[&str],
            default: usize,
        ) -> Result<usize, Box<dyn std::error::Error>> {
            let question = format!("select: {prompt} {{{}}} [{default}]", items.join("|"));
            let reply = self.next(&question);
            self.asked.push(question.clone());
            Ok(match reply {
                Reply::Accept => default,
                Reply::Select(index) => index,
                _ => panic!("script has the wrong kind of reply for: {question}"),
            })
        }

        fn multi_select(
            &mut self,
            prompt: &str,
            items: &[String],
            defaults: &[bool],
        ) -> Result<Vec<usize>, Box<dyn std::error::Error>> {
            let checked: Vec<usize> = defaults
                .iter()
                .enumerate()
                .filter_map(|(i, on)| on.then_some(i))
                .collect();
            let question = format!("multi: {prompt} {{{}}} {checked:?}", items.join("|"));
            let reply = self.next(&question);
            self.asked.push(question.clone());
            Ok(match reply {
                Reply::Accept => checked,
                Reply::Multi(indices) => indices,
                _ => panic!("script has the wrong kind of reply for: {question}"),
            })
        }

        fn input<T>(
            &mut self,
            prompt: &str,
            default: Option<T>,
        ) -> Result<T, Box<dyn std::error::Error>>
        where
            T: Clone + ToString + FromStr,
            <T as FromStr>::Err: ToString,
        {
            let question = match &default {
                Some(value) => format!("input: {prompt} [{}]", value.to_string()),
                None => format!("input: {prompt}"),
            };
            let reply = self.next(&question);
            self.asked.push(question.clone());
            Ok(match reply {
                Reply::Accept => {
                    default.unwrap_or_else(|| panic!("no default to accept for: {question}"))
                }
                Reply::Text(text) => text
                    .parse()
                    .unwrap_or_else(|e: T::Err| panic!("{question}: {}", e.to_string())),
                _ => panic!("script has the wrong kind of reply for: {question}"),
            })
        }

        fn run_shell(&mut self, cmd: &str) -> Result<bool, Box<dyn std::error::Error>> {
            let reply = self.next(cmd);
            self.ran.push(cmd.to_string());
            Ok(match reply {
                Reply::Shell(succeeded) => succeeded,
                _ => panic!("script has the wrong kind of reply for command: {cmd}"),
            })
        }
    }

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    fn bare_system() -> SystemInfo {
        SystemInfo {
            devices: Vec::new(),
            subvolumes: Vec::new(),
            init_system: InitSystemDetected::Systemd,
            package_manager: PackageManager::Pacman,
            deps: Vec::new(),
        }
    }

    fn dep(name: &str, required: bool, installed: bool) -> DepStatus {
        DepStatus {
            name: name.to_string(),
            required,
            path: installed.then(|| format!("/usr/bin/{name}")),
        }
    }

    fn subvol(name: &str, top_level: u64) -> SubvolumeInfo {
        SubvolumeInfo {
            id: 256,
            name: name.to_string(),
            top_level,
        }
    }

    fn device(name: &str, size: &str, serial: Option<&str>, tran: &str) -> BlockDevice {
        BlockDevice {
            name: name.to_string(),
            size: size.to_string(),
            fstype: None,
            serial: serial.map(str::to_string),
            model: None,
            tran: Some(tran.to_string()),
        }
    }

    fn source(label: &str, target_labels: &[&str]) -> Source {
        Source {
            label: label.to_string(),
            volume: "/.btrfs-root".to_string(),
            subvolumes: parse_subvol_list("@,@home"),
            device: "/dev/nvme0n1p2".to_string(),
            snapshot_dir: ".btrbk-snapshots".to_string(),
            target_subdirs: Vec::new(),
            target_labels: target_labels.iter().map(|l| l.to_string()).collect(),
        }
    }

    fn target(label: &str, role: TargetRole) -> Target {
        Target {
            label: label.to_string(),
            serial: format!("SER-{label}"),
            serials: vec![format!("SER-{label}")],
            mount_uuid: None,
            mount: format!("/mnt/{label}"),
            role,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 0,
                yearly: 0,
            },
            display_name: String::new(),
        }
    }

    fn config_with(sources: Vec<Source>, targets: Vec<Target>) -> Config {
        Config {
            sources,
            targets,
            ..Config::default()
        }
    }

    fn names(subvolumes: &[SubvolConfig]) -> Vec<&str> {
        subvolumes.iter().map(|sv| sv.name.as_str()).collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // -----------------------------------------------------------------------
    // Step 1: dependencies
    // -----------------------------------------------------------------------

    const INSTALL_MENU: &str = "select: How would you like to install missing packages? \
         {Install all now (single sudo)|Install one at a time|Skip (I'll install manually)} [0]";

    #[test]
    fn dependencies_all_present_asks_nothing() {
        let mut sys = bare_system();
        sys.deps = vec![dep("btrbk", true, true), dep("mailx", false, true)];
        let mut p = Script::new(vec![]);

        step_dependencies(&mut p, &sys).unwrap();

        assert!(p.asked.is_empty());
        assert!(p.ran.is_empty());
        p.assert_said("[1/9] Checking Dependencies");
        p.assert_said("✓ btrbk (/usr/bin/btrbk)");
        p.assert_said("✓ mailx (/usr/bin/mailx)");
        p.assert_said("All required dependencies are installed.");
        p.assert_not_said("Optional:");
        p.assert_not_said("Missing required");
    }

    #[test]
    fn dependencies_missing_optional_is_a_note_not_a_prompt() {
        let mut sys = bare_system();
        sys.deps = vec![
            dep("btrbk", true, true),
            dep("mailx", false, false),
            dep("smartctl", false, false),
        ];
        let mut p = Script::new(vec![]);

        step_dependencies(&mut p, &sys).unwrap();

        assert!(p.asked.is_empty());
        assert!(p.ran.is_empty());
        p.assert_said("○ mailx (optional)");
        p.assert_said("Note: Optional: mailx, smartctl");
        p.assert_said("All required dependencies are installed.");
        p.assert_not_said("Missing required");
    }

    #[test]
    fn dependencies_install_all_runs_one_command_for_required_then_optional() {
        let mut sys = bare_system();
        sys.deps = vec![
            dep("mailx", false, false),
            dep("btrbk", true, false),
            dep("btrfs", true, false),
            dep("lsblk", true, true),
        ];
        // Enter on the menu takes its default, "Install all now".
        let mut p = Script::new(vec![Reply::Accept, Reply::Shell(true)]);

        step_dependencies(&mut p, &sys).unwrap();

        p.assert_finished();
        assert_eq!(p.asked, [INSTALL_MENU]);
        assert_eq!(
            p.ran,
            [sys.package_manager
                .install_cmd(&["btrbk", "btrfs", "mailx"])]
        );
        p.assert_said("✗ btrbk (required)");
        p.assert_said("Warning: Missing required: btrbk, btrfs");
        p.assert_said("Note: Optional: mailx");
        p.assert_said("All packages installed.");
        p.assert_not_said("Install returned non-zero");
        p.assert_not_said("All required dependencies are installed.");
        p.assert_not_said("Skipping package installation.");
    }

    #[test]
    fn dependencies_install_all_reports_a_failed_install() {
        let mut sys = bare_system();
        sys.package_manager = PackageManager::Apt;
        sys.deps = vec![dep("btrbk", true, false)];
        let mut p = Script::new(vec![Reply::Select(0), Reply::Shell(false)]);

        step_dependencies(&mut p, &sys).unwrap();

        p.assert_finished();
        assert_eq!(p.ran, [sys.package_manager.install_cmd(&["btrbk"])]);
        p.assert_said("Warning: Install returned non-zero. Some packages may need manual install.");
        p.assert_not_said("All packages installed.");
        // Nothing optional is missing, so there is no note about it.
        p.assert_not_said("Optional:");
    }

    #[test]
    fn dependencies_one_at_a_time_installs_only_what_is_confirmed() {
        let mut sys = bare_system();
        sys.deps = vec![
            dep("btrbk", true, false),
            dep("btrfs", true, false),
            dep("mailx", false, false),
        ];
        let mut p = Script::new(vec![
            Reply::Select(1),
            Reply::Accept, // btrbk: default is yes
            Reply::Shell(false),
            Reply::Confirm(false), // btrfs: declined
            Reply::Confirm(true),  // mailx
            Reply::Shell(true),
        ]);

        step_dependencies(&mut p, &sys).unwrap();

        p.assert_finished();
        assert_eq!(
            p.asked,
            [
                INSTALL_MENU,
                "confirm: Install btrbk? [true]",
                "confirm: Install btrfs? [true]",
                "confirm: Install mailx? [true]",
            ]
        );
        assert_eq!(
            p.ran,
            [
                sys.package_manager.install_cmd(&["btrbk"]),
                sys.package_manager.install_cmd(&["mailx"]),
            ]
        );
        p.assert_said("Warning: Failed to install btrbk.");
        p.assert_not_said("Failed to install btrfs.");
        p.assert_not_said("Failed to install mailx.");
        p.assert_not_said("Skipping package installation.");
    }

    #[test]
    fn dependencies_skip_installs_nothing() {
        let mut sys = bare_system();
        sys.deps = vec![dep("btrbk", true, false)];
        let mut p = Script::new(vec![Reply::Select(2)]);

        step_dependencies(&mut p, &sys).unwrap();

        p.assert_finished();
        assert!(p.ran.is_empty());
        p.assert_said("Skipping package installation.");
    }

    // -----------------------------------------------------------------------
    // Step 2: sources
    // -----------------------------------------------------------------------

    #[test]
    fn parse_subvol_list_trims_and_drops_empty_entries() {
        let cases: [(&str, &[&str]); 6] = [
            ("@,@home", &["@", "@home"]),
            (" @ , @home ", &["@", "@home"]),
            ("@,,@home,", &["@", "@home"]),
            ("@, ,@log", &["@", "@log"]),
            ("", &[]),
            (" , ", &[]),
        ];
        for (input, expected) in cases {
            let parsed = parse_subvol_list(input);
            assert_eq!(names(&parsed), expected, "input {input:?}");
            // A typed subvolume is an ordinary scheduled one under btrbk's
            // default snapshot name.
            assert!(parsed.iter().all(|sv| !sv.manual_only));
            assert!(parsed.iter().all(|sv| sv.snapshot_name.is_none()));
        }
    }

    #[test]
    fn backup_candidates_keeps_only_top_level_user_subvolumes() {
        // Each rejected row fails exactly one of the four conditions.
        let detected = [
            subvol("@", 5),
            subvol("@home", 5),
            subvol("@home/bosco/nested", 257),
            subvol("@/.snapshots/12/snapshot", 5),
            subvol("@.archive.20260901T0300", 5),
            subvol("@.btrbk-snapshots", 5),
            subvol("@log", 5),
        ];
        assert_eq!(backup_candidates(&detected), ["@", "@home", "@log"]);
        assert!(backup_candidates(&[]).is_empty());
        assert!(backup_candidates(&[subvol("@", 256)]).is_empty());
    }

    #[test]
    fn resolve_selection_handles_meta_rows_and_the_row_offset() {
        let all = strings(&["@", "@home", "@log", "@opt", "@srv"]);
        let cases: [(&[usize], &[&str]); 9] = [
            // Individual rows: row n is subvolume n - 2.
            (&[2, 4], &["@", "@log"]),
            (&[3], &["@home"]),
            (&[6], &["@srv"]),
            (&[2, 3, 4, 5, 6], &["@", "@home", "@log", "@opt", "@srv"]),
            (&[], &[]),
            // "Select All" overrides whatever else is checked.
            (&[0], &["@", "@home", "@log", "@opt", "@srv"]),
            (&[0, 1, 3], &["@", "@home", "@log", "@opt", "@srv"]),
            // "Deselect All" overrides individually checked rows.
            (&[1], &[]),
            (&[1, 2, 4], &[]),
        ];
        for (selected, expected) in cases {
            assert_eq!(
                resolve_selection(selected, &all),
                expected,
                "selected {selected:?}"
            );
        }
        // A row past the end of the list names nothing.
        assert!(resolve_selection(&[9], &all).is_empty());
    }

    #[test]
    fn sources_entered_by_hand_when_nothing_is_detected() {
        let sys = bare_system();
        let mut config = Config::default();
        let mut p = Script::new(vec![
            Reply::Text("nvme-root"),
            Reply::Text("/.btrfs-nvme"),
            Reply::Text("@, @home,"),
            Reply::Text("/dev/nvme0n1p2"),
            Reply::Confirm(true),
            Reply::Text("hdd"),
            Reply::Text("/hddRaid1"),
            Reply::Text("Projects"),
            Reply::Text("/dev/sda"),
            Reply::Accept, // "Add another source?" defaults to no
        ]);

        step_subvolumes(&mut p, &sys, &mut config).unwrap();

        p.assert_finished();
        let one_source = [
            "input: Source label (e.g. nvme-root)",
            "input: Top-level volume mount (e.g. /.btrfs-nvme)",
            "input: Subvolumes (comma-separated, e.g. @,@home)",
            "input: Device path (e.g. /dev/nvme0n1p2)",
            "confirm: Add another source? [false]",
        ];
        assert_eq!(p.asked, [one_source, one_source].concat());

        assert_eq!(config.sources.len(), 2);
        let first = &config.sources[0];
        assert_eq!(first.label, "nvme-root");
        assert_eq!(first.volume, "/.btrfs-nvme");
        assert_eq!(names(&first.subvolumes), ["@", "@home"]);
        assert_eq!(first.device, "/dev/nvme0n1p2");
        assert_eq!(first.snapshot_dir, ".btrbk-snapshots");
        assert!(first.target_subdirs.is_empty());
        // Empty target_labels = every target; step 3 narrows it where policy says so.
        assert!(first.target_labels.is_empty());
        let second = &config.sources[1];
        assert_eq!(second.label, "hdd");
        assert_eq!(second.volume, "/hddRaid1");
        assert_eq!(names(&second.subvolumes), ["Projects"]);
        assert_eq!(second.device, "/dev/sda");

        p.assert_said("[2/9] Backup Sources (BTRFS Subvolumes)");
        p.assert_said("Note: No BTRFS subvolumes detected. Enter manually.");
        p.assert_said("✓ 2 source(s) configured.");
    }

    #[test]
    fn sources_accepting_every_default_backs_up_all_detected_subvolumes() {
        let mut sys = bare_system();
        sys.subvolumes = vec![
            subvol("@", 5),
            subvol("@.btrbk-snapshots", 5),
            subvol("@home", 5),
            subvol("@home/nested", 258),
        ];
        let mut config = Config::default();
        let mut p = Script::new(vec![
            Reply::Accept,
            Reply::Accept,
            Reply::Text("/dev/nvme0n1p2"),
            Reply::Accept,
            Reply::Accept,
        ]);

        step_subvolumes(&mut p, &sys, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(
            p.asked,
            [
                // Both meta rows start unchecked, every subvolume checked.
                "multi: Select subvolumes to back up \
                 {── Select All ──|── Deselect All ──|@|@home} [2, 3]",
                "input: Top-level volume mount point [/.btrfs-root]",
                "input: Device path for this volume",
                "input: Label for this source [root]",
                "confirm: Add another source volume? [false]",
            ]
        );
        assert_eq!(config.sources.len(), 1);
        let src = &config.sources[0];
        assert_eq!(src.label, "root");
        assert_eq!(src.volume, "/.btrfs-root");
        assert_eq!(src.device, "/dev/nvme0n1p2");
        assert_eq!(names(&src.subvolumes), ["@", "@home"]);
        assert!(src.subvolumes.iter().all(|sv| !sv.manual_only));
        assert!(src.subvolumes.iter().all(|sv| sv.snapshot_name.is_none()));
        assert_eq!(src.snapshot_dir, ".btrbk-snapshots");
        assert!(src.target_subdirs.is_empty());
        assert!(src.target_labels.is_empty());

        p.assert_said("Detected 2 top-level subvolumes:");
        p.assert_said("Selected: @, @home");
        p.assert_said("✓ 1 source(s) configured.");
        p.assert_not_said("No subvolumes selected.");
    }

    #[test]
    fn sources_individual_picks_then_a_second_volume() {
        let mut sys = bare_system();
        sys.subvolumes = vec![subvol("@", 5), subvol("@home", 5), subvol("@log", 5)];
        let mut config = Config::default();
        let mut p = Script::new(vec![
            Reply::Multi(vec![2, 4]),
            Reply::Text("/.btrfs-nvme"),
            Reply::Text("/dev/nvme0n1p2"),
            Reply::Text("nvme"),
            Reply::Confirm(true),
            Reply::Text("hdd"),
            Reply::Text("/hddRaid1"),
            Reply::Text("Projects, ,Audiobooks"),
            Reply::Text("/dev/sda"),
            Reply::Accept,
        ]);

        step_subvolumes(&mut p, &sys, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(
            p.asked[4..],
            [
                "confirm: Add another source volume? [false]",
                "input: Source label",
                "input: Top-level volume mount",
                "input: Subvolumes (comma-separated)",
                "input: Device path",
                "confirm: Add another source volume? [false]",
            ]
        );
        assert_eq!(config.sources.len(), 2);
        assert_eq!(config.sources[0].label, "nvme");
        assert_eq!(config.sources[0].volume, "/.btrfs-nvme");
        assert_eq!(names(&config.sources[0].subvolumes), ["@", "@log"]);
        assert_eq!(config.sources[1].label, "hdd");
        assert_eq!(config.sources[1].volume, "/hddRaid1");
        assert_eq!(config.sources[1].device, "/dev/sda");
        assert_eq!(
            names(&config.sources[1].subvolumes),
            ["Projects", "Audiobooks"]
        );
        assert_eq!(config.sources[1].snapshot_dir, ".btrbk-snapshots");
        assert!(config.sources[1].target_labels.is_empty());
        p.assert_said("Selected: @, @log");
        p.assert_said("✓ 2 source(s) configured.");
    }

    #[test]
    fn sources_deselect_all_configures_no_source_but_still_offers_another_volume() {
        let mut sys = bare_system();
        sys.subvolumes = vec![subvol("@", 5), subvol("@home", 5)];
        let mut config = Config::default();
        let mut p = Script::new(vec![Reply::Multi(vec![1, 2]), Reply::Accept]);

        step_subvolumes(&mut p, &sys, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(p.asked.len(), 2);
        assert_eq!(p.asked[1], "confirm: Add another source volume? [false]");
        assert!(config.sources.is_empty());
        p.assert_said("Warning: No subvolumes selected.");
        p.assert_not_said("Selected:");
        p.assert_said("✓ 0 source(s) configured.");
    }

    #[test]
    fn sources_with_only_snapshot_subvolumes_detected_asks_nothing() {
        let mut sys = bare_system();
        sys.subvolumes = vec![
            subvol("@/.snapshots/1/snapshot", 5),
            subvol("@home/nested", 257),
        ];
        let mut config = Config::default();
        let mut p = Script::new(vec![]);

        step_subvolumes(&mut p, &sys, &mut config).unwrap();

        assert!(p.asked.is_empty());
        assert!(config.sources.is_empty());
        p.assert_said("Note: No user subvolumes found at top-level.");
        p.assert_not_said("source(s) configured");
    }

    // -----------------------------------------------------------------------
    // Step 3: targets
    // -----------------------------------------------------------------------

    #[test]
    fn targets_lists_only_usb_devices() {
        let mut sys = bare_system();
        sys.devices = vec![
            device("sda", "21.8T", Some("ZXA0V0EY"), "sata"),
            device("sdk", "20T", Some("ZXA1NYGZ"), "usb"),
            device("sdl", "1.8T", None, "usb"),
        ];
        let mut config = Config::default();
        let mut p = Script::new(vec![Reply::Confirm(false)]);

        step_targets(&mut p, &sys, &mut config).unwrap();

        p.assert_finished();
        p.assert_said("[3/9] Backup Targets");
        p.assert_said("Detected USB/DAS devices:");
        p.assert_said("• sdk (20T) serial=ZXA1NYGZ");
        p.assert_said("• sdl (1.8T) serial=unknown");
        p.assert_not_said("sda");
        p.assert_not_said("No USB/DAS devices detected");
        p.assert_said("✓ 0 target(s) configured.");
    }

    #[test]
    fn targets_without_usb_devices_says_so() {
        let mut sys = bare_system();
        sys.devices = vec![device("sda", "21.8T", Some("ZXA0V0EY"), "sata")];
        let mut config = Config::default();
        let mut p = Script::new(vec![Reply::Confirm(false)]);

        step_targets(&mut p, &sys, &mut config).unwrap();

        p.assert_said("Note: No USB/DAS devices detected. Enter manually.");
        p.assert_not_said("Detected USB/DAS devices:");
        assert!(config.targets.is_empty());
    }

    #[test]
    fn targets_primary_with_defaults_then_a_mirror() {
        let sys = bare_system();
        let mut config = Config::default();
        let mut p = Script::new(vec![
            Reply::Accept, // no target yet, so adding one is the default
            Reply::Text("primary-22tb"),
            Reply::Text("ZXA1NYGZ"),
            Reply::Text("/mnt/backup-22tb"),
            Reply::Accept, // role: primary
            Reply::Accept, // 4 weeks
            Reply::Accept, // 2 months
            Reply::Confirm(true),
            Reply::Text("recovery-A"),
            Reply::Text("ZK208Q77"),
            Reply::Text("/mnt/backup-system-recovery-A"),
            Reply::Select(1),
            Reply::Text("8"),
            Reply::Text("6"),
            Reply::Accept, // a target exists now, so stopping is the default
        ]);

        step_targets(&mut p, &sys, &mut config).unwrap();

        p.assert_finished();
        let questions = |add_default: bool| {
            vec![
                format!("confirm: Add a backup target? [{add_default}]"),
                "input: Target label (e.g. primary-22tb)".to_string(),
                "input: Drive serial number".to_string(),
                "input: Mount point (e.g. /mnt/backup-22tb)".to_string(),
                "select: Target role {primary|mirror} [0]".to_string(),
                "input: Retention: weeks to keep [4]".to_string(),
                "input: Retention: months to keep [2]".to_string(),
            ]
        };
        let mut expected = questions(true);
        expected.extend(questions(false));
        expected.push("confirm: Add a backup target? [false]".to_string());
        assert_eq!(p.asked, expected);

        assert_eq!(config.targets.len(), 2);
        let primary = &config.targets[0];
        assert_eq!(primary.label, "primary-22tb");
        assert_eq!(primary.serial, "ZXA1NYGZ");
        assert_eq!(primary.serials, ["ZXA1NYGZ"]);
        assert_eq!(primary.mount_uuid, None);
        assert_eq!(primary.mount, "/mnt/backup-22tb");
        assert_eq!(primary.role, TargetRole::Primary);
        assert_eq!(
            primary.retention,
            Retention {
                weekly: 4,
                monthly: 2,
                daily: 0,
                yearly: 0,
            }
        );
        assert_eq!(primary.display_name, "");
        let mirror = &config.targets[1];
        assert_eq!(mirror.label, "recovery-A");
        assert_eq!(mirror.serial, "ZK208Q77");
        assert_eq!(mirror.serials, ["ZK208Q77"]);
        assert_eq!(mirror.mount, "/mnt/backup-system-recovery-A");
        assert_eq!(mirror.role, TargetRole::Mirror);
        assert_eq!(
            mirror.retention,
            Retention {
                weekly: 8,
                monthly: 6,
                daily: 0,
                yearly: 0,
            }
        );
        p.assert_said("✓ 2 target(s) configured.");
    }

    #[test]
    fn targets_already_configured_default_to_adding_none() {
        let sys = bare_system();
        let mut config = Config::default();
        config
            .targets
            .push(target("primary-22tb", TargetRole::Primary));
        let mut p = Script::new(vec![Reply::Accept]);

        step_targets(&mut p, &sys, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(p.asked, ["confirm: Add a backup target? [false]"]);
        assert_eq!(config.targets.len(), 1);
        p.assert_said("✓ 1 target(s) configured.");
    }

    /// The recovery drives are small bootable systems; the audiobook library
    /// would overflow them. An audiobook source with no explicit target list
    /// is therefore pinned to the primary target(s).
    #[test]
    fn targets_pin_audiobook_sources_to_primary_targets() {
        let sys = bare_system();
        let mut config = config_with(
            vec![
                source("AudioBooks-library", &[]),
                source("audiobook-explicit", &["recovery-A"]),
                source("root", &[]),
            ],
            vec![
                target("primary-22tb", TargetRole::Primary),
                target("recovery-A", TargetRole::Mirror),
                target("primary-offsite", TargetRole::Primary),
            ],
        );
        let mut p = Script::new(vec![Reply::Accept]);

        step_targets(&mut p, &sys, &mut config).unwrap();

        assert_eq!(
            config.sources[0].target_labels,
            ["primary-22tb", "primary-offsite"]
        );
        // An explicit choice is left alone, and so is a non-audiobook source.
        assert_eq!(config.sources[1].target_labels, ["recovery-A"]);
        assert!(config.sources[2].target_labels.is_empty());
        p.assert_said(
            "→ Source 'AudioBooks-library' restricted to primary target(s) only: \
             primary-22tb, primary-offsite",
        );
        p.assert_not_said("Source 'audiobook-explicit'");
        p.assert_not_said("Source 'root'");
    }

    #[test]
    fn targets_without_a_primary_leave_audiobook_sources_unrestricted() {
        let sys = bare_system();
        let mut config = config_with(
            vec![source("audiobooks", &[])],
            vec![target("recovery-A", TargetRole::Mirror)],
        );
        let mut p = Script::new(vec![Reply::Accept]);

        step_targets(&mut p, &sys, &mut config).unwrap();

        assert!(config.sources[0].target_labels.is_empty());
        p.assert_not_said("restricted to primary");
    }

    // -----------------------------------------------------------------------
    // Step 4: retention
    // -----------------------------------------------------------------------

    #[test]
    fn retention_is_skipped_without_targets() {
        let mut config = Config::default();
        let mut p = Script::new(vec![]);

        step_retention(&mut p, &mut config).unwrap();

        assert!(p.asked.is_empty());
        p.assert_said("[4/9] Retention Policy");
        p.assert_said("Note: No targets configured — skipping retention.");
        p.assert_not_said("Retention configured.");
    }

    #[test]
    fn retention_left_alone_by_default() {
        let mut config = config_with(vec![], vec![target("primary-22tb", TargetRole::Primary)]);
        let mut p = Script::new(vec![Reply::Accept]);

        step_retention(&mut p, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(
            p.asked,
            ["confirm: Customize retention per target? [false]"]
        );
        assert_eq!(config.targets[0].retention.weekly, 4);
        assert_eq!(config.targets[0].retention.monthly, 2);
        p.assert_said("• primary-22tb: 4 weeks, 2 months");
        p.assert_said("✓ Retention configured.");
        p.assert_not_said("Target: primary-22tb");
    }

    #[test]
    fn retention_customised_per_target_offers_the_current_values() {
        let mut config = config_with(
            vec![],
            vec![
                target("primary-22tb", TargetRole::Primary),
                target("recovery-A", TargetRole::Mirror),
            ],
        );
        config.targets[1].retention.weekly = 1;
        config.targets[1].retention.monthly = 0;
        let mut p = Script::new(vec![
            Reply::Confirm(true),
            Reply::Text("12"),
            Reply::Accept,
            Reply::Accept,
            Reply::Text("3"),
        ]);

        step_retention(&mut p, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(
            p.asked,
            [
                "confirm: Customize retention per target? [false]",
                "input:   Weeks to keep [4]",
                "input:   Months to keep [2]",
                "input:   Weeks to keep [1]",
                "input:   Months to keep [0]",
            ]
        );
        assert_eq!(config.targets[0].retention.weekly, 12);
        assert_eq!(config.targets[0].retention.monthly, 2);
        assert_eq!(config.targets[1].retention.weekly, 1);
        assert_eq!(config.targets[1].retention.monthly, 3);
        p.assert_said("Target: primary-22tb");
        p.assert_said("Target: recovery-A");
    }

    // -----------------------------------------------------------------------
    // Step 5: schedule
    // -----------------------------------------------------------------------

    #[test]
    fn scheduling_records_the_detected_init_system() {
        let cases = [
            (
                InitSystemDetected::Systemd,
                InitSystem::Systemd,
                "systemd (timers)",
            ),
            (
                InitSystemDetected::Openrc,
                InitSystem::Openrc,
                "OpenRC (cron)",
            ),
            (
                InitSystemDetected::Sysvinit,
                InitSystem::Sysvinit,
                "SysVinit (cron)",
            ),
        ];
        for (detected, expected, label) in cases {
            let mut sys = bare_system();
            sys.init_system = detected;
            let mut config = Config::default();
            let mut p = Script::new(vec![Reply::Accept]);

            step_scheduling(&mut p, &sys, &mut config).unwrap();

            p.assert_finished();
            assert_eq!(p.asked, ["confirm: Customize schedule? [false]"]);
            assert_eq!(config.init.system, expected);
            // Declining leaves the schedule as it was.
            assert_eq!(config.schedule.incremental, "03:00");
            assert_eq!(config.schedule.full, "Sun 04:00");
            assert_eq!(config.schedule.randomized_delay_min, 30);
            p.assert_said("[5/9] Backup Schedule");
            p.assert_said(&format!("Init system: {label}"));
            p.assert_said("Incremental: 03:00 daily");
            p.assert_said("Full:        Sun 04:00 weekly");
            p.assert_said(&format!("✓ Schedule configured ({label})."));
        }
    }

    #[test]
    fn scheduling_customised_offers_the_current_values() {
        let sys = bare_system();
        let mut config = Config::default();
        let mut p = Script::new(vec![
            Reply::Confirm(true),
            Reply::Text("01:30"),
            Reply::Accept,
            Reply::Text("45"),
        ]);

        step_scheduling(&mut p, &sys, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(
            p.asked,
            [
                "confirm: Customize schedule? [false]",
                "input: Incremental backup time (HH:MM) [03:00]",
                "input: Full backup schedule (e.g. Sun 04:00) [Sun 04:00]",
                "input: Randomized delay (minutes) [30]",
            ]
        );
        assert_eq!(config.schedule.incremental, "01:30");
        assert_eq!(config.schedule.full, "Sun 04:00");
        assert_eq!(config.schedule.randomized_delay_min, 45);
    }

    // -----------------------------------------------------------------------
    // Step 6: email
    // -----------------------------------------------------------------------

    #[test]
    fn email_declined_turns_reports_off() {
        let mut config = Config::default();
        config.email.enabled = true;
        let mut p = Script::new(vec![Reply::Confirm(false)]);

        step_email(&mut p, &mut config).unwrap();

        p.assert_finished();
        // The offered answer is the current setting.
        assert_eq!(
            p.asked,
            ["confirm: Enable email reports after backup? [true]"]
        );
        assert!(!config.email.enabled);
        p.assert_said("[6/9] Email Notifications");
        p.assert_said("✓ Email: disabled");
    }

    #[test]
    fn email_enabled_on_a_blank_config_offers_the_local_relay() {
        let mut config = Config {
            email: Email {
                enabled: false,
                smtp_host: String::new(),
                smtp_port: 0,
                from: String::new(),
                to: String::new(),
            },
            ..Config::default()
        };
        let mut p = Script::new(vec![
            Reply::Confirm(true),
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
        ]);

        step_email(&mut p, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(
            p.asked,
            [
                "confirm: Enable email reports after backup? [false]",
                "input: Relay host [127.0.0.1]",
                "input: Relay port [25]",
                "input: From address (selects the relay's upstream credential) \
                 [backup@localhost]",
                "input: To address [root@localhost]",
            ]
        );
        assert!(config.email.enabled);
        assert_eq!(config.email.smtp_host, "127.0.0.1");
        assert_eq!(config.email.smtp_port, 25);
        assert_eq!(config.email.from, "backup@localhost");
        assert_eq!(config.email.to, "root@localhost");
        p.assert_said("btrdasd never stores one.");
        p.assert_said("✓ Email: enabled (127.0.0.1)");
    }

    #[test]
    fn email_enabled_on_an_existing_config_offers_its_values() {
        let mut config = Config {
            email: Email {
                enabled: true,
                smtp_host: "relay.lan".to_string(),
                smtp_port: 2525,
                from: "das-backup@thebosco.club".to_string(),
                to: "ops@example.org".to_string(),
            },
            ..Config::default()
        };
        let mut p = Script::new(vec![
            Reply::Accept,
            Reply::Accept,
            Reply::Text("587"),
            Reply::Accept,
            Reply::Text("admin@example.org"),
        ]);

        step_email(&mut p, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(
            p.asked,
            [
                "confirm: Enable email reports after backup? [true]",
                "input: Relay host [relay.lan]",
                "input: Relay port [2525]",
                "input: From address (selects the relay's upstream credential) \
                 [das-backup@thebosco.club]",
                "input: To address [ops@example.org]",
            ]
        );
        assert!(config.email.enabled);
        assert_eq!(config.email.smtp_host, "relay.lan");
        assert_eq!(config.email.smtp_port, 587);
        assert_eq!(config.email.from, "das-backup@thebosco.club");
        assert_eq!(config.email.to, "admin@example.org");
        p.assert_said("✓ Email: enabled (relay.lan)");
    }

    // -----------------------------------------------------------------------
    // Steps 7 and 8: install location, GUI
    // -----------------------------------------------------------------------

    #[test]
    fn install_location_left_alone_by_default() {
        let mut config = Config::default();
        let mut p = Script::new(vec![Reply::Accept]);

        step_install_location(&mut p, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(p.asked, ["confirm: Customize install paths? [false]"]);
        assert_eq!(config.general.install_prefix, "/usr/local");
        assert_eq!(
            config.general.db_path,
            "/var/lib/das-backup/backup-index.db"
        );
        p.assert_said("[7/9] Install Location");
        p.assert_said("Prefix:  /usr/local");
        p.assert_said("DB path: /var/lib/das-backup/backup-index.db");
        p.assert_said("✓ Install paths configured.");
    }

    #[test]
    fn install_location_customised_offers_the_current_paths() {
        let mut config = Config::default();
        let mut p = Script::new(vec![
            Reply::Confirm(true),
            Reply::Text("/usr"),
            Reply::Text("/srv/das/index.db"),
        ]);

        step_install_location(&mut p, &mut config).unwrap();

        p.assert_finished();
        assert_eq!(
            p.asked,
            [
                "confirm: Customize install paths? [false]",
                "input: Install prefix [/usr/local]",
                "input: Database path [/var/lib/das-backup/backup-index.db]",
            ]
        );
        assert_eq!(config.general.install_prefix, "/usr");
        assert_eq!(config.general.db_path, "/srv/das/index.db");
    }

    #[test]
    fn gui_is_opt_in() {
        let mut config = Config::default();
        config.gui.enabled = true;
        let mut p = Script::new(vec![Reply::Accept]);
        step_gui(&mut p, &mut config).unwrap();
        assert_eq!(
            p.asked,
            ["confirm: Install KDE Plasma GUI? (requires Qt6/KF6) [false]"]
        );
        assert!(!config.gui.enabled);
        p.assert_said("[8/9] KDE Plasma GUI");
        p.assert_said("✓ GUI: skipped");

        let mut p = Script::new(vec![Reply::Confirm(true)]);
        step_gui(&mut p, &mut config).unwrap();
        assert!(config.gui.enabled);
        p.assert_said("✓ GUI: will be installed");
    }

    // -----------------------------------------------------------------------
    // Step 9: review
    // -----------------------------------------------------------------------

    fn complete_config() -> Config {
        config_with(
            vec![source("nvme", &[])],
            vec![
                target("primary-22tb", TargetRole::Primary),
                target("recovery-A", TargetRole::Mirror),
            ],
        )
    }

    #[test]
    fn review_of_a_valid_config_is_accepted_by_default() {
        let config = complete_config();
        assert!(config.validate().is_empty());
        let mut p = Script::new(vec![Reply::Accept]);

        step_review(&mut p, &config).unwrap();

        p.assert_finished();
        assert_eq!(p.asked, ["confirm: Proceed with installation? [true]"]);
        p.assert_said("[9/9] Review Configuration");
        p.assert_said("Sources (1):");
        p.assert_said("• nvme [/dev/nvme0n1p2]");
        p.assert_said("volume: /.btrfs-root, subvols: @, @home");
        p.assert_said("Targets (2):");
        p.assert_said("• primary-22tb [SER-primary-22tb] role=primary");
        p.assert_said("mount: /mnt/primary-22tb, retention: 4w 2m");
        p.assert_said("• recovery-A [SER-recovery-A] role=mirror");
        p.assert_said("init: systemd");
        p.assert_said("incremental: 03:00");
        p.assert_said("full: Sun 04:00");
        p.assert_said("Email:\n    disabled");
        p.assert_said("prefix: /usr/local");
        p.assert_said("db: /var/lib/das-backup/backup-index.db");
        p.assert_said("GUI:\n    not installed");
        p.assert_not_said("Validation warnings");
        p.assert_said("✓ Configuration accepted.");
    }

    #[test]
    fn review_shows_validation_warnings_and_can_be_cancelled() {
        let mut config = Config::default();
        config.email.enabled = true;
        config.email.smtp_host = "relay.lan".to_string();
        config.email.smtp_port = 2525;
        config.email.from = "das-backup@thebosco.club".to_string();
        config.email.to = "ops@example.org".to_string();
        config.gui.enabled = true;
        config.init.system = InitSystem::Openrc;
        let warnings = config.validate();
        assert!(!warnings.is_empty());
        let mut p = Script::new(vec![Reply::Confirm(false)]);

        let err = step_review(&mut p, &config).unwrap_err();

        assert_eq!(err.to_string(), "Setup cancelled by user.");
        p.assert_finished();
        p.assert_said("⚠ Validation warnings:");
        for warning in &warnings {
            p.assert_said(&format!("• {warning}"));
        }
        p.assert_said("Sources (0):");
        p.assert_said("Targets (0):");
        p.assert_said("init: openrc");
        p.assert_said("relay.lan:2525 from=das-backup@thebosco.club to=ops@example.org");
        p.assert_said("GUI:\n    will be installed");
        p.assert_not_said("Configuration accepted.");
    }

    // -----------------------------------------------------------------------
    // Whole sessions
    // -----------------------------------------------------------------------

    fn banners(p: &Script) -> Vec<&str> {
        p.said
            .iter()
            .filter_map(|line| line.trim_start().split_once(']'))
            .filter(|(head, _)| head.starts_with('['))
            .map(|(_, title)| title.trim())
            .collect()
    }

    #[test]
    fn first_run_session_produces_the_answered_config() {
        let mut sys = bare_system();
        sys.init_system = InitSystemDetected::Openrc;
        sys.deps = vec![dep("btrbk", true, true)];
        sys.subvolumes = vec![subvol("@", 5), subvol("@home", 5)];
        sys.devices = vec![device("sdk", "20T", Some("ZXA1NYGZ"), "usb")];
        let mut p = Script::new(vec![
            // 2: sources
            Reply::Accept,
            Reply::Accept,
            Reply::Text("/dev/nvme0n1p2"),
            Reply::Accept,
            Reply::Confirm(true),
            Reply::Text("audiobooks"),
            Reply::Text("/hddRaid1"),
            Reply::Text("Audiobooks"),
            Reply::Text("/dev/sda"),
            Reply::Accept,
            // 3: targets
            Reply::Accept,
            Reply::Text("primary-22tb"),
            Reply::Text("ZXA1NYGZ"),
            Reply::Text("/mnt/backup-22tb"),
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Confirm(true),
            Reply::Text("recovery-A"),
            Reply::Text("ZK208Q77"),
            Reply::Text("/mnt/backup-system-recovery-A"),
            Reply::Select(1),
            Reply::Text("1"),
            Reply::Text("0"),
            Reply::Accept,
            // 4-8: retention, schedule, email, install location, GUI
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            // 9: review
            Reply::Accept,
        ]);

        let config = run_wizard_with(&mut p, &sys, None).unwrap();

        p.assert_finished();
        assert_eq!(p.said[0], "\nButteredDASD Setup Wizard");
        assert_eq!(
            banners(&p),
            [
                "Checking Dependencies",
                "Backup Sources (BTRFS Subvolumes)",
                "Backup Targets",
                "Retention Policy",
                "Backup Schedule",
                "Email Notifications",
                "Install Location",
                "KDE Plasma GUI",
                "Review Configuration",
            ]
        );
        for step in 1..=TOTAL_STEPS {
            p.assert_said(&format!("[{step}/9]"));
        }

        assert_eq!(config.sources.len(), 2);
        assert_eq!(config.sources[0].label, "root");
        assert_eq!(names(&config.sources[0].subvolumes), ["@", "@home"]);
        assert!(config.sources[0].target_labels.is_empty());
        assert_eq!(config.sources[1].label, "audiobooks");
        assert_eq!(config.sources[1].target_labels, ["primary-22tb"]);
        assert_eq!(config.targets.len(), 2);
        assert_eq!(config.targets[0].role, TargetRole::Primary);
        assert_eq!(config.targets[1].role, TargetRole::Mirror);
        assert_eq!(config.targets[1].serial, "ZK208Q77");
        assert_eq!(config.targets[1].retention.weekly, 1);
        assert_eq!(config.init.system, InitSystem::Openrc);
        assert!(!config.email.enabled);
        assert!(!config.gui.enabled);
        assert!(config.validate().is_empty());
        p.assert_not_said("Validation warnings");
    }

    #[test]
    fn modify_session_starts_from_the_existing_config() {
        let sys = bare_system();
        let mut existing = complete_config();
        existing.schedule.incremental = "01:15".to_string();
        existing.general.install_prefix = "/usr".to_string();
        // No subvolumes detected, so step 2 asks for a source by hand.
        let mut p = Script::new(vec![
            Reply::Text("hdd"),
            Reply::Text("/hddRaid1"),
            Reply::Text("Projects"),
            Reply::Text("/dev/sda"),
            Reply::Accept,
            Reply::Accept, // targets exist: add none
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
        ]);

        let config = run_wizard_with(&mut p, &sys, Some(existing)).unwrap();

        p.assert_finished();
        assert_eq!(config.sources.len(), 2);
        assert_eq!(config.sources[0].label, "nvme");
        assert_eq!(config.sources[1].label, "hdd");
        assert_eq!(config.targets.len(), 2);
        assert_eq!(config.schedule.incremental, "01:15");
        assert_eq!(config.general.install_prefix, "/usr");
    }

    #[test]
    fn cancelling_at_review_yields_no_config() {
        let sys = bare_system();
        let mut p = Script::new(vec![
            Reply::Text("hdd"),
            Reply::Text("/hddRaid1"),
            Reply::Text("Projects"),
            Reply::Text("/dev/sda"),
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Accept,
            Reply::Confirm(false),
        ]);

        let err = run_wizard_with(&mut p, &sys, Some(complete_config())).unwrap_err();

        assert_eq!(err.to_string(), "Setup cancelled by user.");
        p.assert_finished();
    }

    // -----------------------------------------------------------------------
    // Terminal: the parts that do not need a TTY
    // -----------------------------------------------------------------------

    #[test]
    fn terminal_say_writes_one_line_per_call() {
        let mut terminal = Terminal { out: Vec::new() };
        terminal.say("first");
        terminal.say("");
        terminal.say("\nsecond");
        assert_eq!(
            String::from_utf8(terminal.out).unwrap(),
            "first\n\n\nsecond\n"
        );
    }

    #[test]
    fn terminal_run_shell_reports_the_exit_status() {
        let mut terminal = Terminal { out: Vec::new() };
        assert!(terminal.run_shell("true").unwrap());
        assert!(!terminal.run_shell("false").unwrap());
        // It is a shell command line, not an argv: operators and exit codes work.
        assert!(terminal.run_shell("false || exit 0").unwrap());
        assert!(!terminal.run_shell("true && exit 3").unwrap());
    }

    // -----------------------------------------------------------------------
    // Terminal: without a TTY every prompt must fail, never invent an answer
    // -----------------------------------------------------------------------

    /// Set only in the re-executed child of `no_terminal_means_no_answers`.
    const NO_TTY_CHILD: &str = "BTRDASD_WIZARD_NO_TTY_CHILD";
    const NO_TTY_MARKER: &str = "wizard-no-tty-assertions-ran";

    /// Body of the no-TTY check. It is a no-op in an ordinary test run, where
    /// stderr may well be a terminal and a real prompt would block on the
    /// keyboard; `no_terminal_means_no_answers` re-runs it with all three
    /// standard streams detached.
    #[test]
    fn no_terminal_child() {
        if std::env::var_os(NO_TTY_CHILD).is_none() {
            return;
        }
        let mut terminal = Terminal {
            out: std::io::sink(),
        };
        let not_a_terminal = |err: Box<dyn std::error::Error>| {
            assert!(err.to_string().contains("not a terminal"), "{err}");
        };

        not_a_terminal(terminal.confirm("Proceed?", true).unwrap_err());
        not_a_terminal(terminal.confirm("Proceed?", false).unwrap_err());
        not_a_terminal(
            terminal
                .select("Role", &["primary", "mirror"], 0)
                .unwrap_err(),
        );
        not_a_terminal(
            terminal
                .multi_select("Subvolumes", &strings(&["@", "@home"]), &[true, true])
                .unwrap_err(),
        );
        not_a_terminal(
            terminal
                .input("Label", Some("root".to_string()))
                .unwrap_err(),
        );
        not_a_terminal(terminal.input::<u32>("Weeks", Some(4)).unwrap_err());
        not_a_terminal(terminal.input::<String>("Device", None).unwrap_err());

        // The public entry point: an unanswerable wizard must not hand back a
        // config — least of all the defaults — for `setup` to install.
        not_a_terminal(run_wizard(&bare_system(), None).unwrap_err());
        not_a_terminal(run_wizard(&bare_system(), Some(complete_config())).unwrap_err());

        println!("{NO_TTY_MARKER}");
    }

    #[test]
    fn no_terminal_means_no_answers() {
        let (_, module) = module_path!()
            .split_once("::")
            .expect("test module is nested inside the crate");
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::no_terminal_child")])
            .args(["--nocapture", "--test-threads=1"])
            .env(NO_TTY_CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&child.stdout);
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(
            child.status.success(),
            "child failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        // Guards against the filter matching nothing: a child that ran zero
        // tests also exits 0.
        assert!(
            stdout.contains(NO_TTY_MARKER),
            "child never reached the assertions\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}
