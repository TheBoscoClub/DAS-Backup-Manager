//! The recovery guides rendered from the installed templates into the
//! installation's local docs directory (bd DAS-Backup-Manager-riwa).
//!
//! The package installs `DISASTER-RECOVERY-GUIDE.md` and the other guides as
//! generic templates; a template names each fact this installation decides
//! as `{{name}}` (lower-case letters and `_`). [`render_dir`] fills every
//! name the configuration or a read-only probe supplies and writes the result
//! beside the installation's other documents. A name nothing supplies, or one
//! this version does not know, becomes the visible blank `[FILL IN: name]`,
//! never a guess. `\{{` writes a literal `{{`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::config::{Config, Target, TargetRole};
use crate::local_docs::{DocOutcome, DocReport, Probe};

/// Every name a template may use, with where its value comes from.
pub const VOCABULARY: &[(&str, &str)] = &[
    (
        "primary_target",
        "the primary target's `label` in config.toml",
    ),
    ("primary_uuid", "the primary target's `mount_uuid`"),
    ("primary_mount", "the primary target's `mount`"),
    ("install_user", "`[recovery_os].install_user`"),
    (
        "root_uuid",
        "the UUID of the btrfs mounted at `/` (findmnt)",
    ),
];

/// The one primary target, or `None` when there are none or several: a
/// choice between primaries would be a guess.
fn primary(config: &Config) -> Option<&Target> {
    let mut it = config
        .targets
        .iter()
        .filter(|t| t.role == TargetRole::Primary);
    match (it.next(), it.next()) {
        (Some(t), None) => Some(t),
        _ => None,
    }
}

/// The value of one known name; `Ok(None)` when this installation does not
/// supply it, `Err` when its probe could not run (the guide is then left as
/// it was, never blanked).
fn value(config: &Config, name: &str, probe: &Probe) -> Result<Option<String>, String> {
    let non_empty = |s: &str| (!s.trim().is_empty()).then(|| s.to_string());
    Ok(match name {
        "primary_target" => primary(config).and_then(|t| non_empty(&t.label)),
        "primary_uuid" => primary(config)
            .and_then(|t| t.mount_uuid.as_deref())
            .and_then(non_empty),
        "primary_mount" => primary(config).and_then(|t| non_empty(&t.mount)),
        "install_user" => config
            .recovery_os
            .install_user
            .as_deref()
            .and_then(non_empty),
        "root_uuid" => probe
            .mounts()?
            .into_iter()
            .find(|m| m.target == "/")
            .and_then(|m| non_empty(&m.uuid)),
        _ => None,
    })
}

/// `template` with every `{{name}}` filled.
pub fn render_template(config: &Config, template: &str, probe: &Probe) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find("{{") {
        let (before, tail) = (&rest[..i], &rest[i + 2..]);
        if let Some(kept) = before.strip_suffix('\\') {
            out.push_str(kept);
            out.push_str("{{");
            rest = tail;
            continue;
        }
        out.push_str(before);
        let name = tail
            .find("}}")
            .map(|e| &tail[..e])
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        match name {
            Some(n) => {
                match value(config, n, probe)? {
                    Some(v) => out.push_str(&v),
                    None => out.push_str(&format!("[FILL IN: {n}]")),
                }
                rest = &tail[n.len() + 2..];
            }
            None => {
                out.push_str("{{");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn render_one(
    config: &Config,
    template: &Path,
    dest: &Path,
    write: bool,
    probe: &Probe,
) -> DocOutcome {
    match fs::symlink_metadata(template) {
        Ok(m) if m.is_file() => {}
        Ok(_) => {
            return DocOutcome::Refused("the template is a symlink or not a regular file".into());
        }
        Err(e) => return DocOutcome::Refused(format!("cannot read the template: {e}")),
    }
    let old = match fs::symlink_metadata(dest) {
        Ok(m) if m.file_type().is_symlink() => {
            return DocOutcome::Refused("a symlink, never followed".into());
        }
        Ok(m) if !m.is_file() => return DocOutcome::Refused("not a regular file".into()),
        Ok(_) => match fs::read(dest) {
            Ok(b) => Some(b),
            Err(e) => return DocOutcome::Refused(format!("cannot read it: {e}")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return DocOutcome::Refused(format!("cannot read it: {e}")),
    };
    let text = match fs::read_to_string(template) {
        Ok(t) => t,
        Err(e) => return DocOutcome::Refused(format!("cannot read the template: {e}")),
    };
    let rendered = match render_template(config, &text, probe) {
        Ok(r) => r,
        Err(why) => return DocOutcome::Refused(why),
    };
    let rendered = match crate::local_docs::refreshed(config, &rendered, probe) {
        Ok(Some(filled)) => filled,
        Ok(None) => rendered,
        Err(why) => return DocOutcome::Refused(why),
    };
    if old.as_deref() == Some(rendered.as_bytes()) {
        return DocOutcome::Current;
    }
    if write
        && let Err(e) = crate::fsutil::write_atomic_mode(dest, rendered.as_bytes(), Some(0o644))
    {
        return DocOutcome::Refused(format!("cannot write it: {e}"));
    }
    DocOutcome::Updated
}

/// Render every top-level `*.md` template in `templates` (never `author-*`)
/// into `dir`; the names it handled are returned beside the reports. With
/// `write` false, only say what would change. A template directory that does
/// not exist is an error: the caller decides whether that is absence.
pub fn render_dir(
    config: &Config,
    templates: &Path,
    dir: &Path,
    write: bool,
    probe: &Probe,
) -> Result<(Vec<DocReport>, BTreeSet<String>), String> {
    let listing = fs::read_dir(templates)
        .map_err(|e| format!("cannot list the templates {}: {e}", templates.display()))?;
    let mut names = BTreeSet::new();
    for entry in listing {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", templates.display()))?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.ends_with(".md") && !name.starts_with("author-") && plain_name(&name) {
            // A directory named like a document is not one.
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            names.insert(name);
        }
    }
    let reports = names
        .iter()
        .map(|n| DocReport {
            path: dir.join(n),
            outcome: render_one(config, &templates.join(n), &dir.join(n), write, probe),
        })
        .collect();
    Ok((reports, names))
}

/// The whole refresh of a local docs directory: the guides rendered from
/// `templates` (when given), then every other `*.md` through the marked-block
/// pass. A directory that does not exist has no documents.
pub fn refresh_all(
    config: &Config,
    templates: Option<&Path>,
    dir: &Path,
    write: bool,
    probe: &Probe,
) -> Result<Vec<DocReport>, String> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let (mut reports, handled) = match templates {
        Some(t) => render_dir(config, t, dir, write, probe)?,
        None => (Vec::new(), BTreeSet::new()),
    };
    reports.extend(crate::local_docs::refresh_dir_except(
        config, dir, write, probe, &handled,
    )?);
    reports.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Target, TargetRole};
    use crate::local_docs::DocOutcome;
    use std::fs;

    const UUID: &str = "5eed0016-0000-4000-8000-000000000016";
    const ROOT: &str = "5eed0003-0000-4000-8000-000000000003";

    fn config() -> Config {
        let mut c = Config::default();
        let t: Target = toml::from_str(&format!(
            "label = \"primary-a\"\nserial = \"SERIAL01\"\nmount_uuid = \"{UUID}\"\n\
             mount = \"/mnt/backup-a\"\nrole = \"primary\"\n\
             [retention]\ndaily = 7\n"
        ))
        .unwrap();
        assert_eq!(t.role, TargetRole::Primary);
        c.targets.push(t);
        c.recovery_os.install_user = Some("user".into());
        c
    }

    fn probe() -> Probe<'static> {
        Probe::new(|argv: &[&str]| {
            assert_eq!(argv[0], "findmnt");
            Ok(format!(
                "TARGET=\"/\" UUID=\"{ROOT}\" FSROOT=\"/@\" OPTIONS=\"rw\"\n"
            ))
        })
    }

    fn dead_probe() -> Probe<'static> {
        Probe::new(|argv: &[&str]| panic!("ran {argv:?}"))
    }

    #[test]
    fn config_and_probe_facts_fill_their_names() {
        let t = "t={{primary_target}} u={{primary_uuid}} m={{primary_mount}} \
                 who={{install_user}} root={{root_uuid}}\n";
        let out = render_template(&config(), t, &probe()).unwrap();
        assert_eq!(
            out,
            format!("t=primary-a u={UUID} m=/mnt/backup-a who=user root={ROOT}\n")
        );
    }

    #[test]
    fn what_nothing_supplies_is_a_marked_blank_never_a_guess() {
        let mut c = config();
        c.recovery_os.install_user = None;
        c.targets[0].mount_uuid = None;
        let out = render_template(
            &c,
            "{{install_user}}|{{primary_uuid}}|{{no_such_name}}",
            &dead_probe(),
        )
        .unwrap();
        assert_eq!(
            out,
            "[FILL IN: install_user]|[FILL IN: primary_uuid]|[FILL IN: no_such_name]"
        );
        let mut none = config();
        none.targets.clear();
        assert_eq!(
            render_template(&none, "{{primary_mount}}", &dead_probe()).unwrap(),
            "[FILL IN: primary_mount]"
        );
    }

    #[test]
    fn two_primary_targets_are_not_guessed_between() {
        let mut c = config();
        let mut second = c.targets[0].clone();
        second.label = "primary-b".into();
        c.targets.push(second);
        assert_eq!(
            render_template(&c, "{{primary_target}}", &dead_probe()).unwrap(),
            "[FILL IN: primary_target]"
        );
    }

    #[test]
    fn a_backslash_writes_the_braces_literally_and_angle_placeholders_stay() {
        let out = render_template(
            &config(),
            "\\{{primary_uuid}} and <stamp> and {{ spaced }} and {{Upper}}\n",
            &dead_probe(),
        )
        .unwrap();
        assert_eq!(
            out,
            "{{primary_uuid}} and <stamp> and {{ spaced }} and {{Upper}}\n"
        );
    }

    #[test]
    fn the_root_probe_failing_refuses_the_render_and_a_missing_root_is_blank() {
        let failing = Probe::new(|_: &[&str]| Err("findmnt exited 1".to_string()));
        let err = render_template(&config(), "{{root_uuid}}", &failing).unwrap_err();
        assert!(err.contains("findmnt"), "{err}");
        let no_root = Probe::new(|_: &[&str]| {
            Ok(format!(
                "TARGET=\"/home\" UUID=\"{ROOT}\" FSROOT=\"/@h\" OPTIONS=\"rw\"\n"
            ))
        });
        assert_eq!(
            render_template(&config(), "{{root_uuid}}", &no_root).unwrap(),
            "[FILL IN: root_uuid]"
        );
    }

    fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let (tpl, out) = (d.path().join("tpl"), d.path().join("docs"));
        fs::create_dir(&tpl).unwrap();
        fs::create_dir(&out).unwrap();
        (d, tpl, out)
    }

    #[test]
    fn a_template_whose_marked_block_cannot_be_filled_is_refused_and_not_written() {
        let (_d, tpl, out) = dirs();
        fs::write(
            tpl.join("G.md"),
            "<!-- btrdasd:subvolumes source=nope -->\nx\n<!-- /btrdasd:subvolumes -->\n",
        )
        .unwrap();
        let (reports, _) = render_dir(&config(), &tpl, &out, true, &dead_probe()).unwrap();
        assert!(
            matches!(reports[0].outcome, DocOutcome::Refused(_)),
            "{reports:?}"
        );
        assert!(!out.join("G.md").exists());
    }

    #[test]
    fn rendering_writes_the_guides_and_a_second_run_changes_nothing() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let (_d, tpl, out) = dirs();
        fs::write(tpl.join("GUIDE.md"), "mount {{primary_mount}} as <label>\n").unwrap();
        fs::write(tpl.join("author-x.md"), "{{primary_mount}}\n").unwrap();
        fs::write(tpl.join("notes.txt"), "{{primary_mount}}\n").unwrap();
        fs::create_dir(tpl.join("sub.md")).unwrap();
        fs::write(out.join("author-x.md"), "hand written\n").unwrap();

        let (r, names) = render_dir(&config(), &tpl, &out, true, &dead_probe()).unwrap();
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].outcome, DocOutcome::Updated);
        assert_eq!(names, BTreeSet::from(["GUIDE.md".to_string()]));
        let body = fs::read_to_string(out.join("GUIDE.md")).unwrap();
        assert_eq!(body, "mount /mnt/backup-a as <label>\n");
        assert_eq!(
            fs::read_to_string(out.join("author-x.md")).unwrap(),
            "hand written\n"
        );
        assert!(!out.join("notes.txt").exists() && !out.join("sub.md").exists());
        let meta = fs::metadata(out.join("GUIDE.md")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o644);

        let (again, _) = render_dir(&config(), &tpl, &out, true, &dead_probe()).unwrap();
        assert_eq!(again[0].outcome, DocOutcome::Current);
        assert_eq!(
            meta.ino(),
            fs::metadata(out.join("GUIDE.md")).unwrap().ino(),
            "a current guide is not rewritten"
        );
        let left: Vec<_> = fs::read_dir(&out).unwrap().collect();
        assert_eq!(left.len(), 2, "no temp file left: {left:?}");
    }

    #[test]
    fn a_stale_installation_copy_is_replaced_and_checking_writes_nothing() {
        let (_d, tpl, out) = dirs();
        fs::write(tpl.join("GUIDE.md"), "{{install_user}}\n").unwrap();
        fs::write(out.join("GUIDE.md"), "hand filled in 2026-10-23\n").unwrap();
        let (r, _) = render_dir(&config(), &tpl, &out, false, &dead_probe()).unwrap();
        assert_eq!(r[0].outcome, DocOutcome::Updated);
        assert_eq!(
            fs::read_to_string(out.join("GUIDE.md")).unwrap(),
            "hand filled in 2026-10-23\n"
        );
        render_dir(&config(), &tpl, &out, true, &dead_probe()).unwrap();
        assert_eq!(fs::read_to_string(out.join("GUIDE.md")).unwrap(), "user\n");
    }

    #[test]
    fn a_symlinked_destination_or_template_is_refused_and_never_followed() {
        let (d, tpl, out) = dirs();
        fs::write(tpl.join("GUIDE.md"), "x\n").unwrap();
        let victim = d.path().join("victim");
        fs::write(&victim, "keep\n").unwrap();
        std::os::unix::fs::symlink(&victim, out.join("GUIDE.md")).unwrap();
        let (r, _) = render_dir(&config(), &tpl, &out, true, &dead_probe()).unwrap();
        assert!(
            matches!(&r[0].outcome, DocOutcome::Refused(w) if w.contains("symlink")),
            "{r:?}"
        );
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep\n");

        fs::remove_file(out.join("GUIDE.md")).unwrap();
        fs::remove_file(tpl.join("GUIDE.md")).unwrap();
        fs::write(d.path().join("real.md"), "y\n").unwrap();
        std::os::unix::fs::symlink(d.path().join("real.md"), tpl.join("GUIDE.md")).unwrap();
        let (r, _) = render_dir(&config(), &tpl, &out, true, &dead_probe()).unwrap();
        assert!(matches!(r[0].outcome, DocOutcome::Refused(_)), "{r:?}");
        assert!(!out.join("GUIDE.md").exists());
    }

    #[test]
    fn a_missing_template_directory_is_an_error_not_nothing() {
        let (d, _t, out) = dirs();
        assert!(
            render_dir(
                &config(),
                &d.path().join("absent"),
                &out,
                true,
                &dead_probe()
            )
            .is_err()
        );
    }

    /// The shipped templates: every name is one the render knows, and no
    /// installation-specific blank the render could fill is left as `<...>`.
    #[test]
    fn the_shipped_guides_use_only_names_the_render_knows() {
        let known: Vec<&str> = VOCABULARY.iter().map(|(n, _)| *n).collect();
        for (file, text) in [
            (
                "DISASTER-RECOVERY-GUIDE.md",
                include_str!("../../docs/DISASTER-RECOVERY-GUIDE.md"),
            ),
            (
                "EMERGENCY-QUICK-REFERENCE.md",
                include_str!("../../docs/EMERGENCY-QUICK-REFERENCE.md"),
            ),
            (
                "STORAGE-ARCHITECTURE-AND-RECOVERY.md",
                include_str!("../../docs/STORAGE-ARCHITECTURE-AND-RECOVERY.md"),
            ),
        ] {
            let mut rest = text;
            while let Some(i) = rest.find("{{") {
                if !rest[..i].ends_with('\\') {
                    let tail = &rest[i + 2..];
                    let end = tail
                        .find("}}")
                        .unwrap_or_else(|| panic!("{file}: unclosed brace pair"));
                    let name = &tail[..end];
                    assert!(
                        known.contains(&name),
                        "{file}: {name:?} is not in VOCABULARY"
                    );
                }
                rest = &rest[i + 2..];
            }
            for old in [
                "<primary-label>",
                "<primary-uuid>",
                "<primary-target-uuid>",
                "<backup-target-uuid>",
                "<your-user>",
                "<username>",
                "<your-root-uuid>",
            ] {
                assert!(!text.contains(old), "{file} still has {old}");
            }
            let out = render_template(&config(), text, &probe()).unwrap();
            for n in &known {
                assert!(
                    !out.contains(&format!("[FILL IN: {n}]")),
                    "{file}: {n} is blank though the test config supplies it"
                );
            }
        }
    }

    #[test]
    fn refresh_all_renders_the_guides_then_fills_the_other_documents_once_each() {
        let (_d, tpl, out) = dirs();
        fs::write(tpl.join("GUIDE.md"), "{{install_user}}\n").unwrap();
        fs::write(out.join("GUIDE.md"), "stale\n").unwrap();
        fs::write(out.join("author-x.md"), "hand written\n").unwrap();
        let reports = refresh_all(&config(), Some(&tpl), &out, true, &dead_probe()).unwrap();
        let names: Vec<_> = reports
            .iter()
            .map(|r| r.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["GUIDE.md", "author-x.md"], "{reports:?}");
        assert_eq!(reports[0].outcome, DocOutcome::Updated);
        assert_eq!(reports[1].outcome, DocOutcome::Current);
        assert_eq!(fs::read_to_string(out.join("GUIDE.md")).unwrap(), "user\n");
        // No templates: the directory is refreshed as before; no directory: no documents.
        let again = refresh_all(&config(), None, &out, true, &dead_probe()).unwrap();
        assert_eq!(again.len(), 2);
        let gone = out.join("absent");
        assert!(
            refresh_all(&config(), Some(&tpl), &gone, true, &dead_probe())
                .unwrap()
                .is_empty()
        );
    }
}
