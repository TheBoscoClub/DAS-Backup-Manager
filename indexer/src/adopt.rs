//! Keep `config.toml` in step with the subvolumes that actually exist.
//!
//! The backup used to be an allowlist: a subvolume was backed up only if an
//! entry named it, and `btrfs send` does not descend into nested subvolumes,
//! so a missing entry meant a silently empty directory in every snapshot.
//! This module inverts that. See
//! `docs/superpowers/specs/2026-10-01-subvolume-reconcile-design.md`.

use std::collections::HashSet;

use crate::btrbk_conf::{algorithmic_snapshot_name, resolve_snapshot_names};
use crate::config::{Config, Source, SubvolConfig, TargetRole};
use crate::doctor::glob_match;

/// Whether `path` lies in a snapshot tree — Snapper's `.snapshots` or btrbk's
/// `.btrbk-snapshots`, at any depth. Snapshots are not data to back up; they
/// are copies of data that is.
pub fn in_snapshot_tree(path: &str) -> bool {
    path.split('/')
        .any(|component| component == ".snapshots" || component == ".btrbk-snapshots")
}

/// The first exclude pattern that keeps `path` out, if any. A pattern applies
/// to the subvolume it matches and to everything nested under that
/// subvolume, so excluding `@cache` also excludes `@cache/stremio`.
pub fn excluding_pattern<'a>(path: &str, patterns: &'a [String]) -> Option<&'a str> {
    patterns
        .iter()
        .find(|pattern| {
            // The path itself, then each ancestor: "a/b/c", "a/b", "a".
            let mut candidate = path;
            loop {
                if glob_match(pattern, candidate) {
                    return true;
                }
                match candidate.rfind('/') {
                    Some(cut) => candidate = &candidate[..cut],
                    None => return false,
                }
            }
        })
        .map(String::as_str)
}

/// Subvolume paths from `btrfs subvolume list` output. Each line reads
/// `ID 256 gen 100 top level 5 path <path>`; the path is everything after
/// the first ` path `, because a path may itself contain spaces.
pub fn parse_subvolume_paths(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.split_once(" path "))
        .map(|(_, path)| path.to_string())
        .filter(|path| !path.is_empty())
        .collect()
}

/// What was found on one source volume. `Err` means the volume could not be
/// read and nothing may be concluded about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeListing {
    pub volume: String,
    pub subvolumes: Result<Vec<String>, String>,
}

/// An empty listing is treated as a failed one. Every configured volume holds
/// at least the subvolumes that are being backed up from it, so "nothing
/// here" is the signature of reading the wrong filesystem, and believing it
/// would retire every entry on the volume.
pub fn normalize_listing(volume: &str, listed: Result<Vec<String>, String>) -> VolumeListing {
    let subvolumes = match listed {
        Ok(found) if found.is_empty() => Err(format!(
            "'{volume}' listed no subvolumes — treating the volume as unreadable"
        )),
        other => other,
    };
    VolumeListing {
        volume: volume.to_string(),
        subvolumes,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    SnapshotTree,
    Excluded { pattern: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    pub volume: String,
    pub name: String,
    pub source_label: String,
    pub nested_under: Option<String>,
    pub manual_only: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRef {
    pub source_label: String,
    pub name: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub volume: String,
    pub name: String,
    pub reason: SkipReason,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unplaceable {
    pub volume: String,
    pub name: String,
    pub why: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncPlan {
    pub adopt: Vec<Adoption>,
    pub retire: Vec<EntryRef>,
    pub revive: Vec<EntryRef>,
    pub skipped: Vec<Skip>,
    pub unplaceable: Vec<Unplaceable>,
    pub failed_volumes: Vec<(String, String)>,
}

impl SyncPlan {
    /// Whether applying the plan would alter `config.toml`.
    pub fn changes_config(&self) -> bool {
        !(self.adopt.is_empty() && self.retire.is_empty() && self.revive.is_empty())
    }

    /// Whether the run must be reported as failed: something could not be
    /// read, or something that should be backed up could not be placed.
    pub fn failed(&self) -> bool {
        !(self.failed_volumes.is_empty() && self.unplaceable.is_empty())
    }
}

/// The label of the source that holds adopted subvolumes with no configured
/// ancestor on `volume`: the first source declared for that volume, with
/// `-adopted` appended. `None` if no source uses the volume.
pub fn adoption_source_label(config: &Config, volume: &str) -> Option<String> {
    config
        .sources
        .iter()
        .find(|s| s.volume == volume && !s.label.ends_with("-adopted"))
        .map(|s| format!("{}-adopted", s.label))
}

fn is_nested_under(child: &str, parent: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Compare the config with what was found on each source volume.
pub fn plan_sync(config: &Config, listings: &[VolumeListing]) -> SyncPlan {
    let mut plan = SyncPlan::default();
    let patterns = config.exclude_patterns();
    let has_primary = config.targets.iter().any(|t| t.role == TargetRole::Primary);

    let mut volumes: Vec<&str> = Vec::new();
    for source in &config.sources {
        if !volumes.contains(&source.volume.as_str()) {
            volumes.push(&source.volume);
        }
    }

    for volume in volumes {
        let on_disk = match listings.iter().find(|l| l.volume == volume) {
            Some(VolumeListing {
                subvolumes: Ok(found),
                ..
            }) => found,
            Some(VolumeListing {
                subvolumes: Err(why),
                ..
            }) => {
                plan.failed_volumes.push((volume.to_string(), why.clone()));
                continue;
            }
            None => {
                plan.failed_volumes
                    .push((volume.to_string(), "volume was not listed".to_string()));
                continue;
            }
        };
        let sources: Vec<&Source> = config
            .sources
            .iter()
            .filter(|s| s.volume == volume)
            .collect();

        for source in &sources {
            for entry in &source.subvolumes {
                let present = on_disk.contains(&entry.name);
                let reference = EntryRef {
                    source_label: source.label.clone(),
                    name: entry.name.clone(),
                };
                match (present, entry.retired.is_some()) {
                    (false, false) => plan.retire.push(reference),
                    (true, true) => plan.revive.push(reference),
                    _ => {}
                }
            }
        }

        let mut unknown: Vec<&String> = on_disk
            .iter()
            .filter(|name| {
                !sources
                    .iter()
                    .any(|s| s.subvolumes.iter().any(|e| &e.name == *name))
            })
            .collect();
        unknown.sort();
        unknown.dedup();

        for name in unknown {
            if in_snapshot_tree(name) {
                plan.skipped.push(Skip {
                    volume: volume.to_string(),
                    name: name.clone(),
                    reason: SkipReason::SnapshotTree,
                });
                continue;
            }
            if let Some(pattern) = excluding_pattern(name, &patterns) {
                plan.skipped.push(Skip {
                    volume: volume.to_string(),
                    name: name.clone(),
                    reason: SkipReason::Excluded {
                        pattern: pattern.to_string(),
                    },
                });
                continue;
            }
            // The nearest configured, live ancestor: the longest name that is
            // a path prefix. A retired ancestor is no longer backed up, so it
            // cannot lend its scoping.
            let ancestor = sources
                .iter()
                .flat_map(|s| s.subvolumes.iter().map(move |e| (*s, e)))
                .filter(|(_, e)| e.retired.is_none() && is_nested_under(name, &e.name))
                .max_by_key(|(_, e)| e.name.len());

            match ancestor {
                Some((source, entry)) => plan.adopt.push(Adoption {
                    volume: volume.to_string(),
                    name: name.clone(),
                    source_label: source.label.clone(),
                    nested_under: Some(entry.name.clone()),
                    manual_only: entry.manual_only,
                }),
                None if has_primary => plan.adopt.push(Adoption {
                    volume: volume.to_string(),
                    name: name.clone(),
                    // `sources` is non-empty: the volume came from a source. When
                    // every source here is itself an adoption source, that one is
                    // the home; appending `-adopted` again would invent a second.
                    source_label: adoption_source_label(config, volume)
                        .unwrap_or_else(|| sources[0].label.clone()),
                    nested_under: None,
                    manual_only: false,
                }),
                None => plan.unplaceable.push(Unplaceable {
                    volume: volume.to_string(),
                    name: name.clone(),
                    why: "no target has the primary role, and a subvolume with no \
                          configured parent is sent to the primary only"
                        .to_string(),
                }),
            }
        }
    }
    plan
}

/// `base`, or `base-2`, `base-3`, … — whichever is not in `taken`.
fn unique_name(base: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    // Bounded so a broken predicate fails fast instead of looping forever;
    // 65 534 collisions on one base name cannot occur in a real config.
    (2..=u16::MAX)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("more than 65 000 snapshot names share one base")
}

/// The config that results from carrying out `plan` on `today`. A plan item
/// that cannot be carried out — it names an entry or a volume the config does
/// not have — is an error and no config is returned: a half-applied plan would
/// look complete while leaving a subvolume unprotected.
pub fn apply_plan(config: &Config, plan: &SyncPlan, today: &str) -> Result<Config, String> {
    let mut out = config.clone();

    for (verb, items, retired) in [
        ("retire", &plan.retire, Some(today.to_string())),
        ("revive", &plan.revive, None),
    ] {
        for reference in items {
            let entry = entry_mut(&mut out, reference).ok_or_else(|| {
                format!(
                    "cannot {verb} '{}' in source '{}': no such entry",
                    reference.name, reference.source_label
                )
            })?;
            entry.retired = retired.clone();
        }
    }

    // Every snapshot name in use, so an adopted entry can never collide with
    // one. Existing names are never changed.
    let mut taken: HashSet<String> = out
        .sources
        .iter()
        .flat_map(|s| resolve_snapshot_names(&s.subvolumes))
        .collect();

    for adoption in &plan.adopt {
        let index = match out
            .sources
            .iter()
            .position(|s| s.label == adoption.source_label)
        {
            Some(index) => index,
            None => {
                let model = config
                    .sources
                    .iter()
                    .find(|s| s.volume == adoption.volume)
                    .ok_or_else(|| {
                        format!(
                            "cannot adopt '{}': no source is configured for volume '{}'",
                            adoption.name, adoption.volume
                        )
                    })?;
                let primary: Vec<String> = config
                    .targets
                    .iter()
                    .filter(|t| t.role == TargetRole::Primary)
                    .map(|t| t.label.clone())
                    .take(1)
                    .collect();
                out.sources.push(Source {
                    label: adoption.source_label.clone(),
                    volume: model.volume.clone(),
                    subvolumes: Vec::new(),
                    device: model.device.clone(),
                    snapshot_dir: model.snapshot_dir.clone(),
                    target_subdirs: vec![adoption.source_label.clone()],
                    target_labels: primary,
                });
                out.sources.len() - 1
            }
        };
        let snapshot_name = unique_name(&algorithmic_snapshot_name(&adoption.name), &taken);
        taken.insert(snapshot_name.clone());
        out.sources[index].subvolumes.push(SubvolConfig {
            name: adoption.name.clone(),
            manual_only: adoption.manual_only,
            snapshot_name: Some(snapshot_name),
            adopted: Some(today.to_string()),
            retired: None,
        });
    }
    Ok(out)
}

fn entry_mut<'a>(config: &'a mut Config, reference: &EntryRef) -> Option<&'a mut SubvolConfig> {
    config
        .sources
        .iter_mut()
        .find(|s| s.label == reference.source_label)?
        .subvolumes
        .iter_mut()
        .find(|e| e.name == reference.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn in_snapshot_tree_matches_a_snapshot_directory_at_any_depth() {
        for inside in [
            ".snapshots",
            ".snapshots/4460/snapshot",
            "@root/.snapshots/3811/snapshot",
            ".btrbk-snapshots/root.20260801T0325",
            "Audiobooks/.btrbk-snapshots/foo.20260101T0000",
        ] {
            assert!(in_snapshot_tree(inside), "{inside}");
        }
        for outside in ["@", "@home", "snapshots", "my.snapshots", "a/.snapshotsx/b"] {
            assert!(!in_snapshot_tree(outside), "{outside}");
        }
    }

    #[test]
    fn excluding_pattern_covers_the_path_and_everything_nested_under_it() {
        let p = pats(&["@cache", "coredumps", "scratch-*"]);
        assert_eq!(excluding_pattern("@cache", &p), Some("@cache"));
        assert_eq!(excluding_pattern("@cache/stremio", &p), Some("@cache"));
        assert_eq!(excluding_pattern("@cache/a/b/c", &p), Some("@cache"));
        assert_eq!(excluding_pattern("scratch-1/tmp", &p), Some("scratch-*"));
        // A sibling that merely starts with the same letters is not nested.
        assert_eq!(excluding_pattern("@cache2", &p), None);
        assert_eq!(excluding_pattern("@cachefoo/x", &p), None);
        // Nesting is downward only: a pattern for a child does not exclude its parent.
        assert_eq!(excluding_pattern("a", &pats(&["a/b"])), None);
        assert_eq!(excluding_pattern("@home", &p), None);
        assert_eq!(excluding_pattern("@home", &[]), None);
    }

    #[test]
    fn excluding_pattern_reports_the_first_pattern_that_applies() {
        let p = pats(&["zzz", "@cache*", "@cache"]);
        assert_eq!(excluding_pattern("@cache/x", &p), Some("@cache*"));
    }

    #[test]
    fn parse_subvolume_paths_keeps_spaces() {
        let out = "ID 256 gen 100 top level 5 path @\n\
                   ID 300 gen 9 top level 257 path bosco-media/My Films\n\
                   ID 301 gen 9 top level 5 path a path b\n\
                   \n\
                   garbage line with no marker\n";
        assert_eq!(
            parse_subvolume_paths(out),
            ["@", "bosco-media/My Films", "a path b"]
        );
    }

    #[test]
    fn empty_listing_is_a_failed_listing() {
        let l = normalize_listing("/vol", Ok(Vec::new()));
        assert_eq!(l.volume, "/vol");
        let why = l.subvolumes.unwrap_err();
        assert!(why.contains("no subvolumes"), "{why}");

        let l = normalize_listing("/vol", Err("boom".into()));
        assert_eq!(l.subvolumes.unwrap_err(), "boom");

        let l = normalize_listing("/vol", Ok(vec!["@".into()]));
        assert_eq!(l.subvolumes.unwrap(), ["@"]);
    }

    use crate::config::{Retention, Target};

    fn sv(name: &str) -> SubvolConfig {
        SubvolConfig {
            name: name.into(),
            ..Default::default()
        }
    }

    fn source(label: &str, volume: &str, targets: &[&str], subvols: &[&str]) -> Source {
        Source {
            label: label.into(),
            volume: volume.into(),
            subvolumes: subvols.iter().map(|n| sv(n)).collect(),
            device: format!("UUID=uuid-of-{volume}"),
            snapshot_dir: ".btrbk-snapshots".into(),
            target_subdirs: vec![label.into()],
            target_labels: targets.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn target(label: &str, role: TargetRole) -> Target {
        Target {
            label: label.into(),
            serial: "S".into(),
            serials: vec!["S".into()],
            mount_uuid: None,
            mount: format!("/mnt/{label}"),
            role,
            retention: Retention {
                daily: 7,
                ..Default::default()
            },
            display_name: String::new(),
        }
    }

    /// Two volumes. `/ssd` has two sources (one to everything, one to the
    /// primary only); `/hdd` has one.
    fn config() -> Config {
        let mut c = Config {
            targets: vec![
                target("big", TargetRole::Primary),
                target("small", TargetRole::Mirror),
            ],
            sources: vec![
                source("ssd", "/ssd", &[], &["@srv", "@opt"]),
                source("ssd-vm", "/ssd", &["big"], &["@srv/VirtualMachines"]),
                source("media", "/hdd", &["big"], &["bosco-media"]),
            ],
            ..Config::default()
        };
        c.doctor.exclude = vec!["@cache".into()];
        c
    }

    fn listing(volume: &str, names: &[&str]) -> VolumeListing {
        VolumeListing {
            volume: volume.into(),
            subvolumes: Ok(names.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn all_present() -> Vec<VolumeListing> {
        vec![
            listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines"]),
            listing("/hdd", &["bosco-media"]),
        ]
    }

    #[test]
    fn nothing_to_do_when_disk_and_config_agree() {
        let plan = plan_sync(&config(), &all_present());
        assert_eq!(plan, SyncPlan::default());
        assert!(!plan.changes_config());
        assert!(!plan.failed());
    }

    #[test]
    fn nested_subvolume_joins_its_nearest_configured_ancestors_source() {
        let mut l = all_present();
        l[0] = listing(
            "/ssd",
            &[
                "@srv",
                "@opt",
                "@srv/VirtualMachines",
                "@srv/stremio-web",
                "@srv/VirtualMachines/win11",
            ],
        );
        l[1] = listing("/hdd", &["bosco-media", "bosco-media/video"]);
        let plan = plan_sync(&config(), &l);
        assert_eq!(
            plan.adopt,
            vec![
                Adoption {
                    volume: "/ssd".into(),
                    name: "@srv/VirtualMachines/win11".into(),
                    // Two ancestors are configured; the nearer one wins.
                    source_label: "ssd-vm".into(),
                    nested_under: Some("@srv/VirtualMachines".into()),
                    manual_only: false,
                },
                Adoption {
                    volume: "/ssd".into(),
                    name: "@srv/stremio-web".into(),
                    source_label: "ssd".into(),
                    nested_under: Some("@srv".into()),
                    manual_only: false,
                },
                Adoption {
                    volume: "/hdd".into(),
                    name: "bosco-media/video".into(),
                    source_label: "media".into(),
                    nested_under: Some("bosco-media".into()),
                    manual_only: false,
                },
            ]
        );
        assert!(plan.changes_config());
    }

    #[test]
    fn a_name_that_only_shares_a_prefix_is_not_nested() {
        let mut l = all_present();
        l[0] = listing(
            "/ssd",
            &["@srv", "@opt", "@srv/VirtualMachines", "@srv2", "@optional"],
        );
        let plan = plan_sync(&config(), &l);
        let names: Vec<_> = plan
            .adopt
            .iter()
            .map(|a| (a.name.as_str(), a.nested_under.clone()))
            .collect();
        assert_eq!(names, [("@optional", None), ("@srv2", None)]);
        assert!(plan.adopt.iter().all(|a| a.source_label == "ssd-adopted"));
    }

    #[test]
    fn nested_subvolume_inherits_manual_only() {
        let mut c = config();
        c.sources[0].subvolumes[0].manual_only = true; // @srv
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines", "@srv/x"]);
        assert!(plan_sync(&c, &l).adopt[0].manual_only);
    }

    #[test]
    fn excluded_and_snapshot_tree_subvolumes_are_skipped_with_the_reason() {
        let mut l = all_present();
        l[0] = listing(
            "/ssd",
            &[
                "@srv",
                "@opt",
                "@srv/VirtualMachines",
                "@cache",
                "@cache/stremio",
                ".btrbk-snapshots/opt.20261001T0300",
                "@tmp",
            ],
        );
        let plan = plan_sync(&config(), &l);
        assert!(plan.adopt.is_empty(), "{:?}", plan.adopt);
        assert_eq!(
            plan.skipped,
            vec![
                Skip {
                    volume: "/ssd".into(),
                    name: ".btrbk-snapshots/opt.20261001T0300".into(),
                    reason: SkipReason::SnapshotTree
                },
                Skip {
                    volume: "/ssd".into(),
                    name: "@cache".into(),
                    reason: SkipReason::Excluded {
                        pattern: "@cache".into()
                    }
                },
                Skip {
                    volume: "/ssd".into(),
                    name: "@cache/stremio".into(),
                    reason: SkipReason::Excluded {
                        pattern: "@cache".into()
                    }
                },
                Skip {
                    volume: "/ssd".into(),
                    name: "@tmp".into(),
                    reason: SkipReason::Excluded {
                        pattern: "@tmp".into()
                    }
                },
            ]
        );
        assert!(!plan.changes_config());
    }

    #[test]
    fn a_configured_subvolume_is_never_skipped_even_if_a_pattern_matches_it() {
        let mut c = config();
        c.subvolumes.exclude = vec!["@opt".into()];
        let plan = plan_sync(&c, &all_present());
        assert_eq!(plan, SyncPlan::default());
    }

    #[test]
    fn vanished_subvolume_is_retired_and_a_returning_one_is_revived() {
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@srv/VirtualMachines"]); // @opt gone
        let plan = plan_sync(&config(), &l);
        assert_eq!(
            plan.retire,
            vec![EntryRef {
                source_label: "ssd".into(),
                name: "@opt".into()
            }]
        );
        assert!(plan.revive.is_empty());

        let mut c = config();
        c.sources[0].subvolumes[1].retired = Some("2026-09-01".into()); // @opt
        // Still gone: already retired, nothing to do.
        assert_eq!(plan_sync(&c, &l), SyncPlan::default());
        // Back again: revived, and not adopted a second time.
        let plan = plan_sync(&c, &all_present());
        assert_eq!(
            plan.revive,
            vec![EntryRef {
                source_label: "ssd".into(),
                name: "@opt".into()
            }]
        );
        assert!(plan.adopt.is_empty());
    }

    #[test]
    fn rename_is_a_retire_plus_an_adopt() {
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@opt-new", "@srv/VirtualMachines"]);
        let plan = plan_sync(&config(), &l);
        assert_eq!(
            plan.retire,
            vec![EntryRef {
                source_label: "ssd".into(),
                name: "@opt".into()
            }]
        );
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].name, "@opt-new");
    }

    #[test]
    fn a_retired_ancestor_does_not_place_a_child() {
        let mut c = config();
        c.sources[0].subvolumes[0].retired = Some("2026-09-01".into()); // @srv
        let l = vec![
            listing("/ssd", &["@opt", "@srv/VirtualMachines", "@srv/x"]),
            listing("/hdd", &["bosco-media"]),
        ];
        let plan = plan_sync(&c, &l);
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].nested_under, None);
        assert_eq!(plan.adopt[0].source_label, "ssd-adopted");
    }

    #[test]
    fn a_failed_volume_changes_nothing_on_that_volume_only() {
        let l = vec![
            VolumeListing {
                volume: "/ssd".into(),
                subvolumes: Err("not mounted".into()),
            },
            listing("/hdd", &["bosco-media", "bosco-media/video"]),
        ];
        let plan = plan_sync(&config(), &l);
        assert_eq!(
            plan.failed_volumes,
            vec![("/ssd".into(), "not mounted".into())]
        );
        assert!(
            plan.retire.is_empty(),
            "nothing on /ssd may be retired: {:?}",
            plan.retire
        );
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].volume, "/hdd");
        assert!(plan.failed());
    }

    #[test]
    fn a_volume_with_no_listing_at_all_is_a_failed_volume() {
        let plan = plan_sync(
            &config(),
            &[listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines"])],
        );
        assert_eq!(plan.failed_volumes.len(), 1);
        assert_eq!(plan.failed_volumes[0].0, "/hdd");
        assert!(plan.retire.is_empty());
    }

    #[test]
    fn top_level_subvolume_without_a_primary_target_is_unplaceable() {
        let mut c = config();
        c.targets.retain(|t| t.role != TargetRole::Primary);
        let mut l = all_present();
        l[1] = listing("/hdd", &["bosco-media", "new-top", "bosco-media/video"]);
        let plan = plan_sync(&c, &l);
        assert_eq!(plan.unplaceable.len(), 1);
        assert_eq!(plan.unplaceable[0].name, "new-top");
        assert!(
            plan.unplaceable[0].why.contains("primary"),
            "{}",
            plan.unplaceable[0].why
        );
        // The nested one still has a home.
        assert_eq!(plan.adopt.len(), 1);
        assert!(plan.failed());
    }

    #[test]
    fn apply_adds_entries_with_explicit_unique_names_and_the_date() {
        let mut l = all_present();
        l[0] = listing(
            "/ssd",
            &[
                "@srv",
                "@opt",
                "@srv/VirtualMachines",
                "@srv/stremio-web",
                "opt",
                "@new",
            ],
        );
        let c = config();
        let plan = plan_sync(&c, &l);
        let out = apply_plan(&c, &plan, "2026-10-02").expect("consistent plan");

        let ssd = &out.sources[0];
        let web = ssd
            .subvolumes
            .iter()
            .find(|s| s.name == "@srv/stremio-web")
            .unwrap();
        assert_eq!(web.snapshot_name.as_deref(), Some("srv-stremio-web"));
        assert_eq!(web.adopted.as_deref(), Some("2026-10-02"));

        let adopted = out
            .sources
            .iter()
            .find(|s| s.label == "ssd-adopted")
            .unwrap();
        assert_eq!(adopted.volume, "/ssd");
        assert_eq!(adopted.device, "UUID=uuid-of-/ssd");
        assert_eq!(adopted.snapshot_dir, ".btrbk-snapshots");
        assert_eq!(adopted.target_labels, ["big"]);
        assert_eq!(adopted.target_subdirs, ["ssd-adopted"]);
        let names: Vec<_> = adopted
            .subvolumes
            .iter()
            .map(|s| (s.name.as_str(), s.snapshot_name.as_deref().unwrap()))
            .collect();
        // "@opt" already owns the name "opt", so the bare "opt" gets "opt-2".
        assert_eq!(names, [("@new", "new"), ("opt", "opt-2")]);
        assert!(out.validate().is_empty(), "{:?}", out.validate());
        // The input is untouched.
        assert_eq!(c.sources.len(), 3);
    }

    #[test]
    fn apply_reuses_an_existing_adoption_source() {
        let c = config();
        let mut l = all_present();
        l[0] = listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines", "@one"]);
        let once = apply_plan(&c, &plan_sync(&c, &l), "2026-10-02").expect("consistent plan");
        l[0] = listing(
            "/ssd",
            &["@srv", "@opt", "@srv/VirtualMachines", "@one", "@two"],
        );
        let twice =
            apply_plan(&once, &plan_sync(&once, &l), "2026-10-03").expect("consistent plan");
        assert_eq!(
            twice
                .sources
                .iter()
                .filter(|s| s.label == "ssd-adopted")
                .count(),
            1
        );
        let adopted = twice
            .sources
            .iter()
            .find(|s| s.label == "ssd-adopted")
            .unwrap();
        assert_eq!(adopted.subvolumes.len(), 2);
        // Running the planner on its own result finds nothing more to do.
        assert_eq!(plan_sync(&twice, &l), SyncPlan::default());
    }

    #[test]
    fn apply_stamps_retirement_and_clears_it_on_revival() {
        let c = config();
        let gone = vec![
            listing("/ssd", &["@srv", "@srv/VirtualMachines"]),
            listing("/hdd", &["bosco-media"]),
        ];
        let retired = apply_plan(&c, &plan_sync(&c, &gone), "2026-10-02").expect("consistent plan");
        let opt = retired.sources[0]
            .subvolumes
            .iter()
            .find(|s| s.name == "@opt")
            .unwrap();
        assert_eq!(opt.retired.as_deref(), Some("2026-10-02"));

        let back = apply_plan(&retired, &plan_sync(&retired, &all_present()), "2026-10-09")
            .expect("consistent plan");
        let opt = back.sources[0]
            .subvolumes
            .iter()
            .find(|s| s.name == "@opt")
            .unwrap();
        assert_eq!(opt.retired, None);
    }

    #[test]
    fn a_volume_whose_only_sources_are_adoption_sources_adopts_into_the_existing_one() {
        let mut c = config();
        // Every original entry on /hdd has been retired and removed; only the
        // adoption source is left.
        c.sources[2] = source("media-adopted", "/hdd", &["big"], &["old"]);
        let l = vec![
            listing("/ssd", &["@srv", "@opt", "@srv/VirtualMachines"]),
            listing("/hdd", &["old", "fresh"]),
        ];
        let plan = plan_sync(&c, &l);
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].source_label, "media-adopted");
        assert!(plan.unplaceable.is_empty());

        let out = apply_plan(&c, &plan, "2026-10-02").expect("consistent plan");
        assert_eq!(out.sources.len(), 3, "no second adoption source");
        let names: Vec<_> = out.sources[2]
            .subvolumes
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["old", "fresh"]);
    }

    fn entry(label: &str, name: &str) -> EntryRef {
        EntryRef {
            source_label: label.into(),
            name: name.into(),
        }
    }

    #[test]
    fn apply_refuses_a_retire_or_revive_that_names_no_entry() {
        let c = config();
        for (kind, plan) in [
            (
                "retire",
                SyncPlan {
                    retire: vec![entry("ssd", "@nope")],
                    ..Default::default()
                },
            ),
            (
                "revive",
                SyncPlan {
                    revive: vec![entry("ghost", "@opt")],
                    ..Default::default()
                },
            ),
        ] {
            let why = apply_plan(&c, &plan, "2026-10-02").unwrap_err();
            assert!(why.contains(kind), "{why}");
            let item = &plan.retire.iter().chain(&plan.revive).next().unwrap();
            assert!(
                why.contains(&item.source_label) && why.contains(&item.name),
                "{why}"
            );
        }
    }

    #[test]
    fn apply_refuses_an_adoption_on_a_volume_with_no_source() {
        let plan = SyncPlan {
            adopt: vec![Adoption {
                volume: "/nowhere".into(),
                name: "@lost".into(),
                source_label: "nowhere-adopted".into(),
                nested_under: None,
                manual_only: false,
            }],
            ..Default::default()
        };
        let why = apply_plan(&config(), &plan, "2026-10-02").unwrap_err();
        assert!(why.contains("@lost") && why.contains("/nowhere"), "{why}");
    }
}
