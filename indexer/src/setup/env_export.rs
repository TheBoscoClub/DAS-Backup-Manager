use super::config::*;

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
        if !src.target_subdirs.is_empty() {
            out.push_str(&kv(
                &format!("{p}_TARGET_SUBDIRS"),
                &src.target_subdirs.join(" "),
            ));
        }
    }

    // Targets (indexed)
    out.push_str(&format!("DAS_TARGET_COUNT={}\n", config.targets.len()));
    for (i, tgt) in config.targets.iter().enumerate() {
        let p = format!("DAS_TARGET_{i}");
        out.push_str(&kv(&format!("{p}_LABEL"), &tgt.label));
        out.push_str(&kv(&format!("{p}_SERIAL"), &tgt.serial));
        out.push_str(&kv(&format!("{p}_MOUNT"), &tgt.mount));
        let role_str = match tgt.role {
            TargetRole::Primary => "primary",
            TargetRole::Mirror => "mirror",
            TargetRole::EspSync => "esp-sync",
        };
        out.push_str(&kv(&format!("{p}_ROLE"), role_str));
        if !tgt.display_name.is_empty() {
            out.push_str(&kv(&format!("{p}_DISPLAY_NAME"), &tgt.display_name));
        }
        out.push_str(&format!("{p}_RETENTION_WEEKLY={}\n", tgt.retention.weekly));
        out.push_str(&format!(
            "{p}_RETENTION_MONTHLY={}\n",
            tgt.retention.monthly
        ));
        if tgt.retention.daily > 0 {
            out.push_str(&format!("{p}_RETENTION_DAILY={}\n", tgt.retention.daily));
        }
        if tgt.retention.yearly > 0 {
            out.push_str(&format!("{p}_RETENTION_YEARLY={}\n", tgt.retention.yearly));
        }
    }

    // Convenience: serial map and all target mounts
    let serial_map: Vec<String> = config
        .targets
        .iter()
        .map(|t| format!("{}:{}", t.serial, t.label))
        .collect();
    out.push_str(&kv("DAS_SERIAL_MAP", &serial_map.join(" ")));

    let all_mounts: Vec<&str> = config.targets.iter().map(|t| t.mount.as_str()).collect();
    out.push_str(&kv("DAS_ALL_TARGET_MOUNTS", &all_mounts.join(" ")));

    // ESP
    out.push_str(&format!(
        "DAS_ESP_ENABLED={}\n",
        if config.esp.enabled { "true" } else { "false" }
    ));
    if config.esp.enabled {
        out.push_str(&format!(
            "DAS_ESP_MIRROR={}\n",
            if config.esp.mirror { "true" } else { "false" }
        ));
        out.push_str(&kv("DAS_ESP_PARTITIONS", &config.esp.partitions.join(" ")));
        out.push_str(&kv(
            "DAS_ESP_MOUNT_POINTS",
            &config.esp.mount_points.join(" "),
        ));
    }

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
        config.das.model_pattern = "TDAS".to_string();
        config.boot.archive_retention_days = 365;
        config.sources.push(Source {
            label: "nvme".to_string(),
            volume: "/.btrfs-nvme".to_string(),
            subvolumes: vec![
                SubvolConfig {
                    name: "@".to_string(),
                    manual_only: false,
                    snapshot_name: None,
                },
                SubvolConfig {
                    name: "@home".to_string(),
                    manual_only: false,
                    snapshot_name: None,
                },
            ],
            device: "/dev/nvme0n1p2".to_string(),
            snapshot_dir: ".btrbk-snapshots".to_string(),
            target_subdirs: vec!["nvme".to_string()],
            target_labels: vec![],
        });
        config.targets.push(Target {
            label: "primary-22tb".to_string(),
            serial: "ZXA0LMAE".to_string(),
            mount: "/mnt/backup-22tb".to_string(),
            role: TargetRole::Primary,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 365,
                yearly: 4,
            },
            display_name: "22TB Primary (Bay 2)".to_string(),
        });
        config.targets.push(Target {
            label: "system-2tb".to_string(),
            serial: "ZFL41DNY".to_string(),
            mount: "/mnt/backup-system".to_string(),
            role: TargetRole::EspSync,
            retention: Retention {
                weekly: 4,
                monthly: 2,
                daily: 0,
                yearly: 0,
            },
            display_name: String::new(),
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
        assert!(output.contains("DAS_MODEL_PATTERN='TDAS'"));
        assert!(output.contains("DAS_IO_SCHEDULER='mq-deadline'"));
        assert!(output.contains("DAS_BOOT_ENABLED=true"));
        assert!(output.contains("DAS_BOOT_ARCHIVE_RETENTION_DAYS=365"));
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
        assert!(output.contains("DAS_SOURCE_0_TARGET_SUBDIRS='nvme'"));
    }

    #[test]
    fn dump_env_indexed_targets() {
        let config = test_config();
        let output = dump_env(&config);

        assert!(output.contains("DAS_TARGET_COUNT=2"));
        assert!(output.contains("DAS_TARGET_0_LABEL='primary-22tb'"));
        assert!(output.contains("DAS_TARGET_0_SERIAL='ZXA0LMAE'"));
        assert!(output.contains("DAS_TARGET_0_MOUNT='/mnt/backup-22tb'"));
        assert!(output.contains("DAS_TARGET_0_ROLE='primary'"));
        assert!(output.contains("DAS_TARGET_0_DISPLAY_NAME='22TB Primary (Bay 2)'"));
        assert!(output.contains("DAS_TARGET_0_RETENTION_DAILY=365"));
        assert!(output.contains("DAS_TARGET_0_RETENTION_YEARLY=4"));
        assert!(output.contains("DAS_TARGET_1_LABEL='system-2tb'"));
        assert!(output.contains("DAS_TARGET_1_ROLE='esp-sync'"));
        // display_name empty → not emitted
        assert!(!output.contains("DAS_TARGET_1_DISPLAY_NAME"));
    }

    #[test]
    fn dump_env_convenience_vars() {
        let config = test_config();
        let output = dump_env(&config);

        assert!(output.contains("DAS_SERIAL_MAP='ZXA0LMAE:primary-22tb ZFL41DNY:system-2tb'"));
        assert!(output.contains("DAS_ALL_TARGET_MOUNTS='/mnt/backup-22tb /mnt/backup-system'"));
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
}
