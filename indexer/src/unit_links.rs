//! The systemd units this project writes live in a directory of their own,
//! `<config dir>-units`, and systemd finds each through a symlink of the
//! same name in its unit directory (bd DAS-Backup-Manager-5237.7). Every
//! writer, enabler, disabler and remover of those units goes through here.
//!
//! What systemd does with a linked unit, observed on systemd 262 (the
//! probes are on the bd issue):
//!
//! - `systemctl disable` removes the unit's link as well as its install
//!   links, so a unit that is to stay must be linked again afterwards.
//! - A unit whose link goes at a reload **while it is active** is left
//!   `failed`, which a supervisor restarts. `disable --now` does exactly
//!   that (unlink, reload, then stop), so it is never used on these units:
//!   the stop comes first, and the disable is `--no-reload`.
//! - A drop-in inside the units directory is not read; drop-ins stay in
//!   `<systemd dir>/<name>.d`.

use std::path::{Path, PathBuf};

use crate::fsutil;

/// Runs `systemctl` with these arguments; `Err` carries what it said.
pub type Systemctl<'a> = &'a dyn Fn(&[&str]) -> Result<(), String>;

/// systemd's own directory for the administrator's units.
pub const SYSTEMD_DIR: &str = "/etc/systemd/system";

/// The directory holding the unit files for the config at `config_path`:
/// beside the config's own directory, named `<that directory>-units`. The
/// one place it is derived; never a config key, since root writes and
/// removes files there.
pub fn units_dir(config_path: &Path) -> Result<PathBuf, String> {
    let conf_dir = config_path
        .parent()
        .filter(|d| d.file_name().is_some())
        .ok_or_else(|| {
            format!(
                "{} has no directory beside which to keep the systemd units",
                config_path.display()
            )
        })?;
    let mut name = conf_dir.file_name().unwrap_or_default().to_os_string();
    name.push("-units");
    Ok(conf_dir.with_file_name(name))
}

/// Where a unit's file is and where systemd looks for it.
#[derive(Debug, Clone, Copy)]
pub struct UnitDirs<'a> {
    /// systemd's unit directory ([`SYSTEMD_DIR`], below a root in tests).
    pub systemd: &'a Path,
    /// The units directory ([`units_dir`]).
    pub units: &'a Path,
}

impl UnitDirs<'_> {
    /// The unit's file.
    pub fn file(&self, name: &str) -> PathBuf {
        self.units.join(name)
    }

    /// The name systemd reads it under.
    pub fn link(&self, name: &str) -> PathBuf {
        self.systemd.join(name)
    }
}

/// What stands at a unit's name in systemd's directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Nothing.
    Absent,
    /// A regular file: a unit written before the units directory existed.
    Regular,
    /// The link to this unit's file in the units directory.
    Linked,
}

/// What stands at `name` in systemd's directory, or why it must not be
/// touched: a link to anywhere but this unit's file (a masked unit is a link
/// to /dev/null), a directory or other non-file, or a path that cannot be
/// examined. Nothing is written or removed.
pub fn slot(dirs: &UnitDirs<'_>, name: &str) -> Result<Slot, String> {
    let path = dirs.link(name);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Slot::Absent),
        Err(e) => return Err(format!("cannot examine {}: {e}", path.display())),
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&path)
            .map_err(|e| format!("cannot read the link {}: {e}", path.display()))?;
        if target == dirs.file(name) {
            return Ok(Slot::Linked);
        }
        let way_out = if target == Path::new("/dev/null") {
            format!("the unit is masked; unmask it first (systemctl unmask {name})")
        } else {
            "remove that link first".to_string()
        };
        return Err(format!(
            "{} is a link to {}, not to {}: {way_out}",
            path.display(),
            target.display(),
            dirs.file(name).display()
        ));
    }
    if meta.is_file() {
        Ok(Slot::Regular)
    } else {
        Err(format!("{} is not a file or a link", path.display()))
    }
}

/// The unit's own file may be written: nothing is there, or a regular file.
/// A link there would be written through.
fn file_is_writable(dirs: &UnitDirs<'_>, name: &str) -> Result<(), String> {
    let path = dirs.file(name);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => Ok(()),
        Ok(_) => Err(format!(
            "{} is not a regular file: remove it first",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot examine {}: {e}", path.display())),
    }
}

/// Whether [`place`] would be refused for `name`, without writing anything:
/// for a caller that must refuse before its first write.
pub fn placeable(dirs: &UnitDirs<'_>, name: &str) -> Result<(), String> {
    slot(dirs, name)?;
    file_is_writable(dirs, name)
}

/// Write the unit `name` (root-owned when root writes it) into the units
/// directory and make systemd's name for it the link to that file. Its mode
/// is `mode`; with none it keeps the mode the unit had — its file's, else
/// that of the regular file at systemd's name — and a new unit gets 0644.
/// A regular file at systemd's name — the unit as it was written before —
/// is replaced by the link in one rename, so a timer armed from it stays
/// armed. Refused, with nothing written, as [`slot`] refuses and when the
/// unit's own file is not a regular file. The caller reloads systemd.
pub fn place(dirs: &UnitDirs<'_>, name: &str, text: &str, mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    placeable(dirs, name)?;
    let had = |path: PathBuf| {
        std::fs::symlink_metadata(path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.permissions().mode() & 0o7777)
    };
    let mode = mode
        .or_else(|| had(dirs.file(name)))
        .or_else(|| had(dirs.link(name)))
        .unwrap_or(0o644);
    std::fs::create_dir_all(dirs.units)
        .map_err(|e| format!("cannot create {}: {e}", dirs.units.display()))?;
    fsutil::write_atomic_root_owned(&dirs.file(name), text.as_bytes(), Some(mode))
        .map_err(|e| e.to_string())?;
    make_link(dirs, name)
}

/// Put systemd's name for `name` back after a `disable` removed it: the
/// link to its file. Already that link, or a regular file there (a unit
/// written before the units directory, which `disable` leaves): nothing to
/// do. With nothing at the name and no file in the units directory there is
/// no unit to link, an error. Refused as [`slot`] refuses. The caller
/// reloads systemd.
pub fn link(dirs: &UnitDirs<'_>, name: &str) -> Result<(), String> {
    match slot(dirs, name)? {
        Slot::Linked | Slot::Regular => Ok(()),
        Slot::Absent => {
            let file = dirs.file(name);
            match std::fs::symlink_metadata(&file) {
                Ok(m) if m.is_file() => make_link(dirs, name),
                Ok(_) => Err(format!("{} is not a regular file", file.display())),
                Err(e) => Err(format!("cannot link {name}: {}: {e}", file.display())),
            }
        }
    }
}

/// Make systemd's name for `name` the link to its file, over a regular file
/// there if one is. The caller has checked [`slot`].
fn make_link(dirs: &UnitDirs<'_>, name: &str) -> Result<(), String> {
    if slot(dirs, name)? == Slot::Linked {
        return Ok(());
    }
    std::fs::create_dir_all(dirs.systemd)
        .map_err(|e| format!("cannot create {}: {e}", dirs.systemd.display()))?;
    fsutil::replace_with_symlink(&dirs.link(name), &dirs.file(name)).map_err(|e| e.to_string())?;
    match slot(dirs, name)? {
        Slot::Linked => Ok(()),
        other => Err(format!(
            "{} is {other:?} after linking it to {}",
            dirs.link(name).display(),
            dirs.file(name).display()
        )),
    }
}

/// Turn a timer's schedule off and keep the unit: stop it, disable it
/// without a reload (which removes its link too), link it again, reload.
/// A stop that fails stops here, before anything is unlinked: unlinking an
/// active unit is what leaves it failed. After a failure past the stop the
/// link is still put back and systemd reloaded, and the first error returned.
pub fn keep_disabled(dirs: &UnitDirs<'_>, timer: &str, systemctl: Systemctl) -> Result<(), String> {
    systemctl(&["stop", timer])?;
    let disabled = systemctl(&["--no-reload", "disable", timer]);
    let linked = link(dirs, timer);
    let reloaded = systemctl(&["daemon-reload"]);
    disabled.and(linked).and(reloaded)
}

/// Take units out of systemd before their files are removed: stop each
/// timer among `names`, then disable them all in one call without a reload
/// (removing install links and unit links). Returns what failed; the caller
/// removes the files and reloads. A timer that could not be stopped is
/// still disabled — the caller is removing it — and named.
pub fn retire(names: &[&str], systemctl: Systemctl) -> Vec<String> {
    let mut failures = Vec::new();
    for timer in names.iter().filter(|n| n.ends_with(".timer")) {
        if let Err(e) = systemctl(&["stop", timer]) {
            failures.push(format!("{timer} could not be stopped: {e}"));
        }
    }
    if !names.is_empty() {
        let mut args = vec!["--no-reload", "disable"];
        args.extend_from_slice(names);
        if let Err(e) = systemctl(&args) {
            failures.push(format!("{} may still be enabled: {e}", names.join(" ")));
        }
    }
    failures
}

/// Remove the unit `name`: its file in the units directory, and what stands
/// at systemd's name when that is this unit's link or a regular file (the
/// unit as written before the units directory). Anything else there is left
/// and reported. One already gone is fine. Call after [`retire`].
pub fn remove_files(dirs: &UnitDirs<'_>, name: &str) -> Result<(), String> {
    let gone = |path: PathBuf| match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{} could not be removed: {e}", path.display())),
    };
    let at_systemd = match slot(dirs, name) {
        Ok(Slot::Absent) => Ok(()),
        Ok(Slot::Regular | Slot::Linked) => gone(dirs.link(name)),
        Err(e) => Err(format!("{e}; left in place")),
    };
    let file = gone(dirs.file(name));
    at_systemd.and(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::symlink;

    struct Tree {
        _dir: tempfile::TempDir,
        systemd: PathBuf,
        units: PathBuf,
    }

    impl Tree {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let systemd = dir.path().join("etc/systemd/system");
            let units = dir.path().join("etc/das-backup-units");
            std::fs::create_dir_all(&systemd).unwrap();
            Tree {
                _dir: dir,
                systemd,
                units,
            }
        }

        fn dirs(&self) -> UnitDirs<'_> {
            UnitDirs {
                systemd: &self.systemd,
                units: &self.units,
            }
        }

        /// The link at systemd's name, as text; panics unless it is a link.
        fn link_target(&self, name: &str) -> PathBuf {
            std::fs::read_link(self.systemd.join(name)).unwrap()
        }
    }

    struct Calls {
        seen: RefCell<Vec<String>>,
        failing: &'static str,
    }

    impl Calls {
        fn new(failing: &'static str) -> Self {
            Calls {
                seen: RefCell::new(Vec::new()),
                failing,
            }
        }

        fn run(&self, args: &[&str]) -> Result<(), String> {
            let line = args.join(" ");
            let fail = !self.failing.is_empty() && line.contains(self.failing);
            self.seen.borrow_mut().push(line.clone());
            if fail {
                Err(format!("systemctl {line} failed"))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn the_units_directory_is_beside_the_configs_own() {
        assert_eq!(
            units_dir(Path::new("/etc/das-backup/config.toml")),
            Ok(PathBuf::from("/etc/das-backup-units"))
        );
        assert!(units_dir(Path::new("/config.toml")).is_err());
        assert!(units_dir(Path::new("config.toml")).is_err());
    }

    #[test]
    fn a_unit_is_written_into_the_units_directory_and_linked() {
        let t = Tree::new();
        place(&t.dirs(), "das-x.timer", "one\n", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(t.units.join("das-x.timer")).unwrap(),
            "one\n"
        );
        assert_eq!(t.link_target("das-x.timer"), t.units.join("das-x.timer"));
        assert_eq!(slot(&t.dirs(), "das-x.timer"), Ok(Slot::Linked));
        // Again, with new text: the file changes, the link is still the link.
        place(&t.dirs(), "das-x.timer", "two\n", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(t.units.join("das-x.timer")).unwrap(),
            "two\n"
        );
        assert_eq!(t.link_target("das-x.timer"), t.units.join("das-x.timer"));
        assert!(
            std::fs::symlink_metadata(t.units.join("das-x.timer"))
                .unwrap()
                .is_file()
        );
    }

    #[test]
    fn a_unit_written_before_as_a_regular_file_becomes_the_link() {
        let t = Tree::new();
        std::fs::write(t.systemd.join("das-x.service"), "old\n").unwrap();
        assert_eq!(slot(&t.dirs(), "das-x.service"), Ok(Slot::Regular));
        place(&t.dirs(), "das-x.service", "new\n", None).unwrap();
        assert_eq!(
            t.link_target("das-x.service"),
            t.units.join("das-x.service")
        );
        assert_eq!(
            std::fs::read_to_string(t.systemd.join("das-x.service")).unwrap(),
            "new\n"
        );
        // No temp name is left beside it.
        let left: Vec<_> = std::fs::read_dir(&t.systemd)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["das-x.service"]);
    }

    #[test]
    fn a_unit_keeps_the_mode_it_had_unless_one_is_given() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let t = Tree::new();
        // New: 0644. Its old regular file's mode crosses into the units directory.
        place(&t.dirs(), "das-x.timer", "one\n", None).unwrap();
        assert_eq!(mode(t.units.join("das-x.timer")), 0o644);
        std::fs::write(t.systemd.join("das-y.timer"), "old\n").unwrap();
        std::fs::set_permissions(
            t.systemd.join("das-y.timer"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        place(&t.dirs(), "das-y.timer", "new\n", None).unwrap();
        assert_eq!(mode(t.units.join("das-y.timer")), 0o600);
        // Written again it keeps its own; a given mode wins.
        place(&t.dirs(), "das-y.timer", "newer\n", None).unwrap();
        assert_eq!(mode(t.units.join("das-y.timer")), 0o600);
        place(&t.dirs(), "das-y.timer", "newest\n", Some(0o644)).unwrap();
        assert_eq!(mode(t.units.join("das-y.timer")), 0o644);
    }

    #[test]
    fn a_masked_unit_or_a_foreign_link_is_refused_with_nothing_written() {
        for target in ["/dev/null", "/opt/other/das-x.timer"] {
            let t = Tree::new();
            symlink(target, t.systemd.join("das-x.timer")).unwrap();
            let err = place(&t.dirs(), "das-x.timer", "text\n", None).unwrap_err();
            assert!(err.contains(target), "{err}");
            assert_eq!(
                err.contains("systemctl unmask das-x.timer"),
                target == "/dev/null",
                "{err}"
            );
            assert!(!t.units.exists(), "{target}: the units directory was made");
            assert_eq!(t.link_target("das-x.timer"), PathBuf::from(target));
            assert!(link(&t.dirs(), "das-x.timer").is_err());
            assert!(remove_files(&t.dirs(), "das-x.timer").is_err());
            assert_eq!(t.link_target("das-x.timer"), PathBuf::from(target));
        }
        // A directory at the name is no unit either.
        let t = Tree::new();
        std::fs::create_dir(t.systemd.join("das-x.timer")).unwrap();
        assert!(place(&t.dirs(), "das-x.timer", "text\n", None).is_err());
        assert!(!t.units.exists());
    }

    #[test]
    fn a_link_where_the_units_own_file_belongs_is_never_written_through() {
        let t = Tree::new();
        std::fs::create_dir_all(&t.units).unwrap();
        let elsewhere = t.units.join("elsewhere");
        std::fs::write(&elsewhere, "keep\n").unwrap();
        symlink(&elsewhere, t.units.join("das-x.timer")).unwrap();
        let err = place(&t.dirs(), "das-x.timer", "text\n", None).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "keep\n");
        assert_eq!(slot(&t.dirs(), "das-x.timer"), Ok(Slot::Absent));
    }

    #[test]
    fn a_schedule_turned_off_stops_then_disables_without_a_reload_then_links_again() {
        let t = Tree::new();
        place(&t.dirs(), "das-x.timer", "text\n", None).unwrap();
        // As systemctl disable does to a linked unit: the link goes.
        let systemd = t.systemd.clone();
        let calls = Calls::new("");
        let run = |args: &[&str]| {
            if args.contains(&"disable") {
                std::fs::remove_file(systemd.join("das-x.timer")).unwrap();
            }
            calls.run(args)
        };
        keep_disabled(&t.dirs(), "das-x.timer", &run).unwrap();
        assert_eq!(
            *calls.seen.borrow(),
            [
                "stop das-x.timer",
                "--no-reload disable das-x.timer",
                "daemon-reload"
            ]
        );
        assert_eq!(t.link_target("das-x.timer"), t.units.join("das-x.timer"));
    }

    #[test]
    fn linking_again_leaves_an_old_regular_unit_alone_and_needs_a_file_to_link() {
        let t = Tree::new();
        // A host not upgraded yet: disable leaves the regular file, and it
        // must not become a link to a file that is not there.
        std::fs::write(t.systemd.join("das-x.timer"), "old\n").unwrap();
        let calls = Calls::new("");
        keep_disabled(&t.dirs(), "das-x.timer", &|a| calls.run(a)).unwrap();
        assert_eq!(slot(&t.dirs(), "das-x.timer"), Ok(Slot::Regular));
        assert_eq!(
            std::fs::read_to_string(t.systemd.join("das-x.timer")).unwrap(),
            "old\n"
        );
        // Nothing at the name and no file: nothing to link.
        let err = link(&t.dirs(), "das-y.timer").unwrap_err();
        assert!(err.contains("das-y.timer"), "{err}");
        assert_eq!(slot(&t.dirs(), "das-y.timer"), Ok(Slot::Absent));
    }

    #[test]
    fn a_timer_that_cannot_be_stopped_is_not_unlinked() {
        let t = Tree::new();
        place(&t.dirs(), "das-x.timer", "text\n", None).unwrap();
        let calls = Calls::new("stop");
        let err = keep_disabled(&t.dirs(), "das-x.timer", &|a| calls.run(a)).unwrap_err();
        assert!(err.contains("stop das-x.timer"), "{err}");
        assert_eq!(*calls.seen.borrow(), ["stop das-x.timer"]);
    }

    #[test]
    fn a_failed_disable_still_leaves_the_unit_linked_and_reloaded() {
        let t = Tree::new();
        place(&t.dirs(), "das-x.timer", "text\n", None).unwrap();
        let systemd = t.systemd.clone();
        let calls = Calls::new("disable");
        let run = |args: &[&str]| {
            if args.contains(&"disable") {
                std::fs::remove_file(systemd.join("das-x.timer")).unwrap();
            }
            calls.run(args)
        };
        let err = keep_disabled(&t.dirs(), "das-x.timer", &run).unwrap_err();
        assert!(err.contains("disable"), "{err}");
        assert_eq!(calls.seen.borrow().last().unwrap(), "daemon-reload");
        assert_eq!(t.link_target("das-x.timer"), t.units.join("das-x.timer"));
    }

    #[test]
    fn retiring_stops_each_timer_then_disables_everything_in_one_call_without_a_reload() {
        let calls = Calls::new("");
        let failures = retire(&["das-x.timer", "das-x.service", "das-y.timer"], &|a| {
            calls.run(a)
        });
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(
            *calls.seen.borrow(),
            [
                "stop das-x.timer",
                "stop das-y.timer",
                "--no-reload disable das-x.timer das-x.service das-y.timer"
            ]
        );
        // A stop that fails is reported, and the disable still follows.
        let calls = Calls::new("stop");
        let failures = retire(&["das-x.timer"], &|a| calls.run(a));
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(calls.seen.borrow().len(), 2);
        // Nothing to retire asks nothing of systemd.
        let calls = Calls::new("");
        assert!(retire(&[], &|a| calls.run(a)).is_empty());
        assert!(calls.seen.borrow().is_empty());
    }

    /// A path that cannot be examined is never "nothing there": each lookup
    /// tells "not found" from any other failure. Here the failure is
    /// ENOTDIR, a directory on the way that is a regular file.
    #[test]
    fn a_path_that_cannot_be_examined_is_refused_never_read_as_absent() {
        let t = Tree::new();
        let not_a_dir = t.systemd.join("plain");
        std::fs::write(&not_a_dir, "x").unwrap();
        // systemd's directory unreadable: no slot, nothing placed or removed.
        let blind = UnitDirs {
            systemd: &not_a_dir,
            units: &t.units,
        };
        let err = slot(&blind, "das-x.timer").unwrap_err();
        assert!(err.contains("cannot examine"), "{err}");
        assert!(place(&blind, "das-x.timer", "text\n", None).is_err());
        assert!(!t.units.exists(), "nothing was written");
        // The units directory unreadable: the unit's own file is not writable,
        // and removing it is an error, not "already gone".
        let blind = UnitDirs {
            systemd: &t.systemd,
            units: &not_a_dir,
        };
        let err = placeable(&blind, "das-x.timer").unwrap_err();
        assert!(err.contains("cannot examine"), "{err}");
        let err = remove_files(&blind, "das-x.timer").unwrap_err();
        assert!(err.contains("could not be removed"), "{err}");
    }

    /// Linking again needs a regular file to link to: a directory where the
    /// unit's file belongs is not one.
    #[test]
    fn linking_again_refuses_a_non_file_where_the_units_file_belongs() {
        let t = Tree::new();
        std::fs::create_dir_all(t.units.join("das-x.timer")).unwrap();
        let err = link(&t.dirs(), "das-x.timer").unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        assert_eq!(slot(&t.dirs(), "das-x.timer"), Ok(Slot::Absent));
    }

    #[test]
    fn removing_a_unit_takes_its_file_and_its_link_or_its_old_regular_file() {
        let t = Tree::new();
        place(&t.dirs(), "das-x.timer", "text\n", None).unwrap();
        remove_files(&t.dirs(), "das-x.timer").unwrap();
        assert!(!t.units.join("das-x.timer").exists());
        assert_eq!(slot(&t.dirs(), "das-x.timer"), Ok(Slot::Absent));
        // Already gone: fine.
        remove_files(&t.dirs(), "das-x.timer").unwrap();
        // The unit as it was written before the units directory.
        std::fs::write(t.systemd.join("das-y.timer"), "old\n").unwrap();
        remove_files(&t.dirs(), "das-y.timer").unwrap();
        assert_eq!(slot(&t.dirs(), "das-y.timer"), Ok(Slot::Absent));
    }
}
