use super::config::*;

/// The local recovery documents' directory for the config at `config_path`,
/// as one `dump-env` line: `DAS_RECOVERY_OS_LOCAL_DOCS_DIR`, resolved by
/// [`local_docs_dir`] (bd DAS-Backup-Manager-5237.6), or, when it cannot be
/// resolved, `DAS_RECOVERY_OS_LOCAL_DOCS_ERROR` saying why. Only provision
/// reads either; the scripts never derive the directory themselves.
pub fn local_docs_env(config: &Config, config_path: &std::path::Path) -> String {
    match local_docs_dir(config, config_path) {
        Ok(dir) => kv("DAS_RECOVERY_OS_LOCAL_DOCS_DIR", &dir.to_string_lossy()),
        Err(why) => kv("DAS_RECOVERY_OS_LOCAL_DOCS_ERROR", &why),
    }
}

/// Export config as shell-sourceable KEY=VALUE pairs with DAS_ prefix.
/// Output format uses indexed arrays for sources/targets.
pub fn dump_env(config: &Config) -> String {
    let mut out = String::with_capacity(4096);

    // General
    out.push_str(&kv("DAS_VERSION", &config.general.version));
    out.push_str(&kv("DAS_INSTALL_PREFIX", &config.general.install_prefix));
    out.push_str(&kv("DAS_DB_PATH", &config.general.db_path));
    out.push_str(&kv("DAS_LOG_FILE", &config.general.log_file));
    out.push_str(&kv("DAS_GROWTH_LOG", &config.general.growth_log));
    out.push_str(&kv("DAS_LAST_REPORT", &config.general.last_report));
    out.push_str(&kv("DAS_BTRBK_CONF", &config.general.btrbk_conf));
    out.push_str(&kv(
        "DAS_BTRBK_STATE_CONF",
        &buttered_dasd::btrbk_conf::state_conf_path(config).to_string_lossy(),
    ));

    // Init
    let init_str = match config.init.system {
        InitSystem::Systemd => "systemd",
        InitSystem::Sysvinit => "sysvinit",
        InitSystem::Openrc => "openrc",
    };
    out.push_str(&kv("DAS_INIT_SYSTEM", init_str));

    // Schedule
    out.push_str(&kv(
        "DAS_SCHEDULE_INCREMENTAL",
        &config.schedule.incremental,
    ));
    out.push_str(&kv("DAS_SCHEDULE_FULL", &config.schedule.full));
    out.push_str(&format!(
        "DAS_SCHEDULE_DELAY_MIN={}\n",
        config.schedule.randomized_delay_min
    ));

    // DAS enclosure
    out.push_str(&kv("DAS_MODEL_PATTERN", &config.das.model_pattern));
    out.push_str(&format!(
        "DAS_EXPECTED_LINK_MBIT_S={}\n",
        config.das.expected_link_mbit_s
    ));
    out.push_str(&kv("DAS_IO_SCHEDULER", &config.das.io_scheduler));
    out.push_str(&kv("DAS_MOUNT_OPTS", &config.das.mount_opts));

    // Boot
    out.push_str(&format!(
        "DAS_BOOT_ENABLED={}\n",
        if config.boot.enabled { "true" } else { "false" }
    ));
    out.push_str(&kv(
        "DAS_BOOT_SUBVOLUMES",
        &config.boot.subvolumes.join(" "),
    ));
    out.push_str(&format!(
        "DAS_BOOT_ARCHIVE_RETENTION_DAYS={}\n",
        config.boot.archive_retention_days
    ));

    // Scrub
    out.push_str(&format!(
        "DAS_SCRUB_ENABLED={}\n",
        if config.scrub.enabled {
            "true"
        } else {
            "false"
        }
    ));
    out.push_str(&kv("DAS_SCRUB_ON_CALENDAR", &config.scrub.on_calendar));
    // The effective list: [scrub].targets, else every [[target]] (bd dis3).
    out.push_str(&kv(
        "DAS_SCRUB_TARGETS",
        &config.scrub_target_labels().join(" "),
    ));
    out.push_str(&format!(
        "DAS_SCRUB_WARN_AGE_DAYS={}\n",
        config.scrub.warn_age_days
    ));
    out.push_str(&format!(
        "DAS_SCRUB_FAIL_AGE_DAYS={}\n",
        config.scrub.fail_age_days
    ));

    // Recovery OS: the egress an unattended update must have (bd 6pd3).
    // Absent when unset, so the script can tell "not configured" from a value.
    if let Some(org) = &config.recovery_os.egress_org {
        out.push_str(&kv("DAS_RECOVERY_OS_EGRESS_ORG", org));
    }
    // The account provision creates on a rebuilt drive; absent when unset, so
    // provision can refuse "not configured".
    if let Some(user) = &config.recovery_os.install_user {
        out.push_str(&kv("DAS_RECOVERY_OS_INSTALL_USER", user));
    }

    // Sources (indexed)
    out.push_str(&format!("DAS_SOURCE_COUNT={}\n", config.sources.len()));
    for (i, src) in config.sources.iter().enumerate() {
        let p = format!("DAS_SOURCE_{i}");
        out.push_str(&kv(&format!("{p}_LABEL"), &src.label));
        out.push_str(&kv(&format!("{p}_VOLUME"), &src.volume));
        out.push_str(&kv(&format!("{p}_DEVICE"), &src.device));
        let subvol_names: Vec<&str> = src.subvolumes.iter().map(|sv| sv.name.as_str()).collect();
        out.push_str(&kv(&format!("{p}_SUBVOLUMES"), &subvol_names.join(" ")));
        out.push_str(&kv(&format!("{p}_SNAPSHOT_DIR"), &src.snapshot_dir));
    }

    // Targets (indexed)
    out.push_str(&format!("DAS_TARGET_COUNT={}\n", config.targets.len()));
    for (i, tgt) in config.targets.iter().enumerate() {
        let p = format!("DAS_TARGET_{i}");
        let effective = tgt.effective_serials();
        let primary_serial = effective.first().cloned().unwrap_or_default();
        out.push_str(&kv(&format!("{p}_LABEL"), &tgt.label));
        // Legacy single-serial — read by health/SMART code paths and existing
        // shell scripts that have not yet migrated to the SERIALS list form.
        out.push_str(&kv(&format!("{p}_SERIAL"), &primary_serial));
        // Full list of expected RAID-1 member serials (space-separated).
        // backup-run.sh uses this to issue per-serial-absence warnings without
        // aborting — a missing serial is degraded, not failed.
        out.push_str(&kv(&format!("{p}_SERIALS"), &effective.join(" ")));
        // BTRFS filesystem UUID for mount-by-UUID. Empty when unset; backup-run.sh
        // falls back to device-by-serial when empty.
        out.push_str(&kv(
            &format!("{p}_MOUNT_UUID"),
            tgt.mount_uuid.as_deref().unwrap_or(""),
        ));
        out.push_str(&kv(&format!("{p}_MOUNT"), &tgt.mount));
        let role_str = match tgt.role {
            TargetRole::Primary => "primary",
            TargetRole::Mirror => "mirror",
        };
        out.push_str(&kv(&format!("{p}_ROLE"), role_str));
        if !tgt.display_name.is_empty() {
            out.push_str(&kv(&format!("{p}_DISPLAY_NAME"), &tgt.display_name));
        }
    }

    // Convenience: serial map and all target mounts.
    // Map preserves legacy single-serial form (anchor:label) for compatibility;
    // multi-leg targets contribute one entry per expected serial.
    let serial_map: Vec<String> = config
        .targets
        .iter()
        .flat_map(|t| {
            t.effective_serials()
                .into_iter()
                .map(move |s| format!("{}:{}", s, t.label))
        })
        .collect();
    out.push_str(&kv("DAS_SERIAL_MAP", &serial_map.join(" ")));

    let all_mounts: Vec<&str> = config.targets.iter().map(|t| t.mount.as_str()).collect();
    out.push_str(&kv("DAS_ALL_TARGET_MOUNTS", &all_mounts.join(" ")));

    // Email
    out.push_str(&format!(
        "DAS_EMAIL_ENABLED={}\n",
        if config.email.enabled {
            "true"
        } else {
            "false"
        }
    ));
    if config.email.enabled {
        out.push_str(&kv("DAS_EMAIL_SMTP_HOST", &config.email.smtp_host));
        out.push_str(&format!("DAS_EMAIL_SMTP_PORT={}\n", config.email.smtp_port));
        out.push_str(&kv("DAS_EMAIL_FROM", &config.email.from));
        out.push_str(&kv("DAS_EMAIL_TO", &config.email.to));
    }

    out
}

/// Format a single KEY="VALUE" line, shell-escaping the value.
fn kv(key: &str, value: &str) -> String {
    // Replace single quotes with '\'' for safe shell quoting
    let escaped = value.replace('\'', "'\\''");
    format!("{key}='{escaped}'\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        let mut config = Config::default();
        config.general.install_prefix = "/usr/local".to_string();
        config.das.model_pattern = "ExampleDAS".to_string();
        config.boot.archive_retention_days = 365;
        config.sources.push(Source {
            label: "nvme".to_string(),
            volume: "/.btrfs-nvme".to_string(),
            subvolumes: vec![
                SubvolConfig {
                    name: "@".to_string(),
                    snapshot_name: None,
                    ..Default::default()
                },
                SubvolConfig {
                    name: "@home".to_string(),
                    snapshot_name: None,
                    ..Default::default()
                },
            ],
            device: "/dev/nvme0n1p2".to_string(),
            snapshot_dir: ".btrbk-snapshots".to_string(),
            target_subdirs: vec!["nvme".to_string()],
            target_labels: vec![],
            static_subvolumes: Vec::new(),
        });
        config.targets.push(Target {
            label: "primary-22tb".to_string(),
            serial: "SERIAL08".to_string(),
            serials: vec!["SERIAL08".to_string(), "SERIAL11".to_string()],
            mount_uuid: Some("5eed0007-0000-4000-8000-000000000007".to_string()),
            mount: "/mnt/backup-22tb".to_string(),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 365,
                yearly: 4,
            },
            display_name: "22TB Primary (Bay 2)".to_string(),
            esp_partuuid: None,
        });
        config.targets.push(Target {
            label: "system-recovery-B-2tb".to_string(),
            serial: "SERIAL02".to_string(),
            serials: vec!["SERIAL02".to_string()],
            mount_uuid: None,
            mount: "/mnt/backup-system-recovery-B".to_string(),
            role: TargetRole::Mirror,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 0,
                yearly: 0,
            },
            display_name: String::new(),
            esp_partuuid: None,
        });
        config
    }

    #[test]
    fn dump_env_contains_expected_keys() {
        let config = test_config();
        let output = dump_env(&config);

        assert!(output.contains("DAS_VERSION="));
        assert!(output.contains("DAS_DB_PATH="));
        assert!(output.contains("DAS_LOG_FILE="));
        assert!(output.contains("DAS_BTRBK_CONF="));
        assert!(output.contains("DAS_MODEL_PATTERN='ExampleDAS'"));
        assert!(output.contains("DAS_IO_SCHEDULER='mq-deadline'"));
        assert!(output.contains("DAS_BOOT_ENABLED=true"));
        assert!(output.contains("DAS_BOOT_ARCHIVE_RETENTION_DAYS=365"));
        assert!(output.contains("DAS_SCRUB_ENABLED=true"));
        assert!(output.contains("DAS_SCRUB_ON_CALENDAR='*-*-01 03:05:00'"));
        // No [scrub].targets: every configured [[target]], in order (bd dis3).
        assert!(output.contains("DAS_SCRUB_TARGETS='primary-22tb system-recovery-B-2tb'"));
        assert!(output.contains("DAS_SCRUB_WARN_AGE_DAYS=45"));
        assert!(output.contains("DAS_SCRUB_FAIL_AGE_DAYS=75"));
    }

    #[test]
    fn dump_env_exports_the_state_conf_path() {
        let mut config = test_config();
        config.general.btrbk_conf = "/etc/btrbk/btrbk.conf".to_string();
        let output = dump_env(&config);
        assert!(
            output.contains("DAS_BTRBK_STATE_CONF='/etc/btrbk/btrbk-state.conf'\n"),
            "{output}"
        );
    }

    #[test]
    fn dump_env_indexed_sources() {
        let config = test_config();
        let output = dump_env(&config);

        assert!(output.contains("DAS_SOURCE_COUNT=1"));
        assert!(output.contains("DAS_SOURCE_0_LABEL='nvme'"));
        assert!(output.contains("DAS_SOURCE_0_VOLUME='/.btrfs-nvme'"));
        assert!(output.contains("DAS_SOURCE_0_DEVICE='/dev/nvme0n1p2'"));
        assert!(output.contains("DAS_SOURCE_0_SUBVOLUMES='@ @home'"));
        assert!(output.contains("DAS_SOURCE_0_SNAPSHOT_DIR='.btrbk-snapshots'"));
        // Target directories come from btrbk.conf's `target` lines now
        // (bd DAS-Backup-Manager-c0x): nothing reads a subdir list.
        assert!(!output.contains("_TARGET_SUBDIRS"));
    }

    #[test]
    fn dump_env_indexed_targets() {
        let config = test_config();
        let output = dump_env(&config);

        assert!(output.contains("DAS_TARGET_COUNT=2"));
        assert!(output.contains("DAS_TARGET_0_LABEL='primary-22tb'"));
        assert!(output.contains("DAS_TARGET_0_SERIAL='SERIAL08'"));
        assert!(output.contains("DAS_TARGET_0_SERIALS='SERIAL08 SERIAL11'"));
        assert!(output.contains("DAS_TARGET_0_MOUNT_UUID='5eed0007-0000-4000-8000-000000000007'"));
        assert!(output.contains("DAS_TARGET_0_MOUNT='/mnt/backup-22tb'"));
        assert!(output.contains("DAS_TARGET_0_ROLE='primary'"));
        assert!(output.contains("DAS_TARGET_0_DISPLAY_NAME='22TB Primary (Bay 2)'"));
        assert!(output.contains("DAS_TARGET_1_LABEL='system-recovery-B-2tb'"));
        assert!(output.contains("DAS_TARGET_1_SERIALS='SERIAL02'"));
        // mount_uuid not set on the 2tb recovery copy — emits empty string
        assert!(output.contains("DAS_TARGET_1_MOUNT_UUID=''"));
        assert!(output.contains("DAS_TARGET_1_ROLE='mirror'"));
        // display_name empty → not emitted
        assert!(!output.contains("DAS_TARGET_1_DISPLAY_NAME"));
    }

    #[test]
    fn dump_env_convenience_vars() {
        let config = test_config();
        let output = dump_env(&config);

        // Multi-leg targets contribute one entry per expected serial.
        assert!(output.contains(
            "DAS_SERIAL_MAP='SERIAL08:primary-22tb SERIAL11:primary-22tb SERIAL02:system-recovery-B-2tb'"
        ));
        assert!(
            output
                .contains("DAS_ALL_TARGET_MOUNTS='/mnt/backup-22tb /mnt/backup-system-recovery-B'")
        );
    }

    #[test]
    fn dump_env_is_valid_shell() {
        let config = test_config();
        let output = dump_env(&config);

        // Every non-empty line should be a valid assignment (KEY=VALUE or KEY='VALUE')
        for line in output.lines() {
            if line.is_empty() {
                continue;
            }
            assert!(line.contains('='), "line missing assignment: {line}");
            let key = line.split('=').next().unwrap();
            assert!(
                key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "invalid key chars: {key}"
            );
        }
    }

    #[test]
    fn kv_escapes_single_quotes() {
        let result = kv("TEST", "it's a test");
        assert_eq!(result, "TEST='it'\\''s a test'\n");
    }

    /// The egress an unattended recovery-drive update must have: exported only
    /// when configured, so the script can refuse "not configured" (bd 6pd3).
    #[test]
    fn dump_env_exports_the_recovery_os_egress_org_only_when_set() {
        let mut config = test_config();
        assert!(!dump_env(&config).contains("DAS_RECOVERY_OS_EGRESS_ORG"));
        config.recovery_os.egress_org = Some("AS64500".into());
        assert!(dump_env(&config).contains("DAS_RECOVERY_OS_EGRESS_ORG='AS64500'\n"));
    }

    /// No built-in enclosure: an unset `[das].model_pattern` is exported
    /// empty, which the scripts read as "no enclosure filter".
    #[test]
    fn dump_env_exports_an_empty_model_pattern_by_default() {
        assert!(dump_env(&Config::default()).contains("DAS_MODEL_PATTERN=''\n"));
    }

    /// The link rate `backup-run.sh` holds the enclosure to: USB 3.2 Gen2 by
    /// default, configuration otherwise (bd DAS-Backup-Manager-7qe).
    #[test]
    fn dump_env_exports_the_expected_link_rate() {
        assert!(dump_env(&Config::default()).contains("DAS_EXPECTED_LINK_MBIT_S=10000\n"));
        let mut config = Config::default();
        config.das.expected_link_mbit_s = 5000;
        assert!(dump_env(&config).contains("DAS_EXPECTED_LINK_MBIT_S=5000\n"));
        let parsed: Das = toml::from_str("expected_link_mbit_s = 20000\n").expect("parse");
        assert_eq!(parsed.expected_link_mbit_s, 20000);
        let unset: Das = toml::from_str("").expect("parse");
        assert_eq!(unset.expected_link_mbit_s, 10000);
    }

    /// The recovery OS's account: exported only when configured, so provision
    /// can refuse "not configured".
    #[test]
    fn dump_env_exports_the_recovery_os_install_user_only_when_set() {
        let mut config = test_config();
        assert!(!dump_env(&config).contains("DAS_RECOVERY_OS_INSTALL_USER"));
        config.recovery_os.install_user = Some("recovery".into());
        assert!(dump_env(&config).contains("DAS_RECOVERY_OS_INSTALL_USER='recovery'\n"));
    }

    /// The local recovery documents' directory, resolved once here for the
    /// config at its path (bd DAS-Backup-Manager-5237.6): the scripts never
    /// derive it. A config it cannot be resolved for exports the reason under
    /// another name instead, so provision can refuse saying why while every
    /// other reader of `dump-env` (the nightly backup) is unaffected.
    #[test]
    fn local_docs_env_exports_the_resolved_directory_or_why_not() {
        let mut config = test_config();
        let conf = std::path::Path::new("/etc/das-backup/config.toml");
        assert_eq!(
            local_docs_env(&config, conf),
            "DAS_RECOVERY_OS_LOCAL_DOCS_DIR='/etc/das-backup-docs'\n"
        );
        config.recovery_os.local_docs_dir = Some("/etc/das-backup/docs".into());
        let out = local_docs_env(&config, conf);
        assert!(!out.contains("DAS_RECOVERY_OS_LOCAL_DOCS_DIR="), "{out}");
        assert!(
            out.starts_with("DAS_RECOVERY_OS_LOCAL_DOCS_ERROR='") && out.contains("inside"),
            "{out}"
        );
    }

    /// Retention reaches btrbk through the rendered btrbk.conf; no script reads
    /// it from the environment, so none is exported (bd DAS-Backup-Manager-9yj).
    #[test]
    fn dump_env_exports_no_per_target_retention() {
        let mut config = test_config();
        config.targets[1].retention.daily = 1;
        config.targets[1].retention.yearly = 1;
        let output = dump_env(&config);
        assert!(output.contains("DAS_TARGET_1_LABEL="));
        assert!(
            !output
                .lines()
                .any(|l| l.starts_with("DAS_TARGET_") && l.contains("_RETENTION_")),
            "{output}"
        );
    }
}
