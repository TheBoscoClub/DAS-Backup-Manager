use crate::config::{Config, SubvolConfig};

/// Information about a subvolume within a source.
#[derive(Debug, Clone)]
pub struct SubvolInfo {
    pub source_label: String,
    pub name: String,
}

/// List all subvolumes across all sources.
pub fn list_subvolumes(config: &Config) -> Vec<SubvolInfo> {
    config
        .sources
        .iter()
        .flat_map(|src| {
            src.subvolumes.iter().map(|sv| SubvolInfo {
                source_label: src.label.clone(),
                name: sv.name.clone(),
            })
        })
        .collect()
}

/// Add a subvolume to a source.
pub fn add_subvolume(config: &mut Config, source_label: &str, name: &str) -> Result<(), String> {
    let src = config
        .sources
        .iter_mut()
        .find(|s| s.label == source_label)
        .ok_or_else(|| format!("Source '{}' not found", source_label))?;
    if src.subvolumes.iter().any(|sv| sv.name == name) {
        return Err(format!(
            "Subvolume '{}' already exists in source '{}'",
            name, source_label
        ));
    }
    src.subvolumes.push(SubvolConfig {
        name: name.to_string(),
        ..Default::default()
    });
    Ok(())
}

/// Remove a subvolume from a source.
pub fn remove_subvolume(config: &mut Config, source_label: &str, name: &str) -> Result<(), String> {
    let src = config
        .sources
        .iter_mut()
        .find(|s| s.label == source_label)
        .ok_or_else(|| format!("Source '{}' not found", source_label))?;
    let len_before = src.subvolumes.len();
    src.subvolumes.retain(|sv| sv.name != name);
    if src.subvolumes.len() == len_before {
        return Err(format!(
            "Subvolume '{}' not found in source '{}'",
            name, source_label
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Source;

    fn test_config() -> Config {
        let mut cfg = Config::default();
        cfg.sources.push(Source {
            label: "nvme".into(),
            volume: "/.btrfs-nvme".into(),
            subvolumes: vec![
                SubvolConfig {
                    name: "@".into(),
                    snapshot_name: None,
                    ..Default::default()
                },
                SubvolConfig {
                    name: "@home".into(),
                    snapshot_name: None,
                    ..Default::default()
                },
            ],
            device: "/dev/nvme0n1p2".into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec!["nvme".into()],
            target_labels: vec![],
            static_subvolumes: Vec::new(),
        });
        cfg.sources.push(Source {
            label: "ssd".into(),
            volume: "/.btrfs-ssd".into(),
            subvolumes: vec![SubvolConfig {
                name: "@opt".into(),
                snapshot_name: None,
                ..Default::default()
            }],
            device: "/dev/sdb".into(),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec!["ssd".into()],
            target_labels: vec![],
            static_subvolumes: Vec::new(),
        });
        cfg
    }

    #[test]
    fn list_all_subvolumes() {
        let cfg = test_config();
        let svs = list_subvolumes(&cfg);
        assert_eq!(svs.len(), 3);
        assert_eq!(svs[0].source_label, "nvme");
        assert_eq!(svs[0].name, "@");
        assert_eq!(svs[1].name, "@home");
        assert_eq!(svs[2].source_label, "ssd");
        assert_eq!(svs[2].name, "@opt");
    }

    #[test]
    fn add_subvolume_success() {
        let mut cfg = test_config();
        add_subvolume(&mut cfg, "nvme", "@log").unwrap();
        assert_eq!(cfg.sources[0].subvolumes.len(), 3);
        assert_eq!(cfg.sources[0].subvolumes[2].name, "@log");
    }

    #[test]
    fn add_subvolume_duplicate_fails() {
        let mut cfg = test_config();
        let result = add_subvolume(&mut cfg, "nvme", "@");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already exists"));
    }

    #[test]
    fn add_subvolume_bad_source_fails() {
        let mut cfg = test_config();
        let result = add_subvolume(&mut cfg, "nonexistent", "@foo");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn remove_subvolume_success() {
        let mut cfg = test_config();
        remove_subvolume(&mut cfg, "nvme", "@home").unwrap();
        assert_eq!(cfg.sources[0].subvolumes.len(), 1);
        assert_eq!(cfg.sources[0].subvolumes[0].name, "@");
    }

    #[test]
    fn remove_subvolume_not_found_fails() {
        let mut cfg = test_config();
        let result = remove_subvolume(&mut cfg, "nvme", "@nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }
}
