//! The backup units `cmake --install` used to write, and the one-time cleanup
//! of them that `btrdasd setup --upgrade` does (bd DAS-Backup-Manager-7rf).
//!
//! Until 7rf two writers made the same four units: CMake installed
//! `das-backup{,-full}.{service,timer}` under `<prefix>/lib/systemd/system`,
//! and `btrdasd setup` writes its own to `/etc/systemd/system`. systemd runs
//! the `/etc` copy, so the installed one lay dormant until that copy went —
//! `setup --uninstall` removes it — and then ran with two hazards setup's units
//! never had: `ConditionPathExistsGlob=/dev/disk/by-id/usb-*` skipped a run,
//! with no report and no history row, whenever no USB disk was attached, and
//! `TimeoutStartSec=21600` killed a run still waiting behind a scrub after six
//! hours. Setup is now the only writer, and an upgrade removes what older
//! versions left: only at the paths CMake wrote to, and only a file whose bytes
//! are a version this project installed. Anything else there — an edited file,
//! a link, a directory, a copy reached through a linked directory, a file
//! replaced while it was being checked — stays where it is, and is named.
//!
//! Remove this module once no supported host can still have those files, as
//! the legacy entries on `setup --uninstall-all`'s list will go.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// What CMake's `configure_file(... @ONLY)` filled in the two service
/// templates: `DAS_SCRIPT_DIR`, which was `${CMAKE_INSTALL_PREFIX}/lib/das-backup`
/// from the first release to the last.
const SCRIPT_DIR_PLACEHOLDER: &str = "@DAS_SCRIPT_DIR@";

/// Every version of the four units CMake installed under `lib/systemd/system`:
/// from ab46e1d (2026-02-22; every release from v0.5.1 to v0.7.22.3 shipped it)
/// to 83874bf, the last. Each file is the blob git holds at that commit — the
/// services as templates, the timers as installed (they never changed).
const SHIPPED: [(&str, &str); 12] = [
    (
        "das-backup.service",
        include_str!("retired_units/das-backup.service.in.ab46e1d"),
    ),
    (
        "das-backup.service",
        include_str!("retired_units/das-backup.service.in.d28da12"),
    ),
    (
        "das-backup.service",
        include_str!("retired_units/das-backup.service.in.4be84af"),
    ),
    (
        "das-backup.service",
        include_str!("retired_units/das-backup.service.in.00e40c8"),
    ),
    (
        "das-backup.service",
        include_str!("retired_units/das-backup.service.in.83874bf"),
    ),
    (
        "das-backup-full.service",
        include_str!("retired_units/das-backup-full.service.in.ab46e1d"),
    ),
    (
        "das-backup-full.service",
        include_str!("retired_units/das-backup-full.service.in.d28da12"),
    ),
    (
        "das-backup-full.service",
        include_str!("retired_units/das-backup-full.service.in.4be84af"),
    ),
    (
        "das-backup-full.service",
        include_str!("retired_units/das-backup-full.service.in.00e40c8"),
    ),
    (
        "das-backup-full.service",
        include_str!("retired_units/das-backup-full.service.in.83874bf"),
    ),
    (
        "das-backup.timer",
        include_str!("retired_units/das-backup.timer.8ea5c74"),
    ),
    (
        "das-backup-full.timer",
        include_str!("retired_units/das-backup-full.timer.8ea5c74"),
    ),
];

/// The four units.
const UNITS: [&str; 4] = [
    "das-backup.service",
    "das-backup-full.service",
    "das-backup.timer",
    "das-backup-full.timer",
];

/// Where CMake installed them, below the root: `<prefix>/lib/systemd/system`
/// for the two prefixes systemd reads units under — `/usr`, which packages and
/// this project's own installs used, and `/usr/local`, CMake's default. A copy
/// under any other prefix is never loaded, and `setup --uninstall-all` still
/// lists it.
const UNIT_DIRS: [&str; 2] = ["usr/lib/systemd/system", "usr/local/lib/systemd/system"];

/// Longer than any version: a file past this is not one, and is not read.
const MAX_LEN: u64 = 64 * 1024;

/// Open flags: never through a link, and never blocking — a pipe at the path
/// opens at once instead of waiting for a writer, and its `fstat` then refuses
/// it. Single, distinct bits, so `+` is `|` here (asserted below), written `+`
/// for the reason `recovery_os`'s `FILE_FLAGS` gives: a mutation test cannot
/// tell `|` from `^` on disjoint bits, while every mutation of `+` changes it.
const OPEN_FLAGS: i32 = libc::O_NOFOLLOW + libc::O_NONBLOCK;
const _: () = assert!(libc::O_NOFOLLOW & libc::O_NONBLOCK == 0);

/// Whether `bytes` is `template` as `configure_file(... @ONLY)` wrote it: the
/// same bytes but for the one placeholder, which became an absolute
/// `<prefix>/lib/das-backup`. A template without a placeholder must match
/// exactly.
fn configured_from(bytes: &[u8], template: &str) -> bool {
    let Some((head, tail)) = template.split_once(SCRIPT_DIR_PLACEHOLDER) else {
        return bytes == template.as_bytes();
    };
    bytes.len() >= head.len() + tail.len()
        && bytes.starts_with(head.as_bytes())
        && bytes.ends_with(tail.as_bytes())
        && is_script_dir(&bytes[head.len()..bytes.len() - tail.len()])
}

/// Whether `dir` could have been `DAS_SCRIPT_DIR`: an absolute path ending in
/// `/lib/das-backup`, with no space or control character in it.
fn is_script_dir(dir: &[u8]) -> bool {
    dir.starts_with(b"/")
        && dir.ends_with(b"/lib/das-backup")
        && !dir
            .iter()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
}

/// Whether `bytes` is a version of `unit` this project installed.
fn is_shipped(unit: &str, bytes: &[u8]) -> bool {
    SHIPPED
        .iter()
        .any(|&(name, template)| name == unit && configured_from(bytes, template))
}

/// What is at a retired unit's path, as far as reading it goes.
#[derive(Debug)]
enum Candidate {
    Absent,
    /// A link, a directory, a pipe: nothing CMake installed.
    NotAFile,
    /// A regular file longer than [`MAX_LEN`].
    TooLong,
    /// What a regular file holds, and the descriptor it was read through. The
    /// descriptor stays open until the file is removed: while it is, the inode
    /// it names cannot be freed and handed to another file, so that inode
    /// number tells this file from a replacement.
    Bytes(Vec<u8>, File),
}

/// Read the file at `path`, never opening a link at `path` itself and never
/// reading past [`MAX_LEN`]. A link in a directory above `path` is followed
/// here, and refused before the removal: see [`link_above`]. An error is any
/// failure other than nothing being there or `path` being a link.
fn read_candidate(path: &Path) -> io::Result<Candidate> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_FLAGS)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Candidate::Absent),
        // What O_NOFOLLOW answers for a link.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(Candidate::NotAFile),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Ok(Candidate::NotAFile);
    }
    if meta.len() > MAX_LEN {
        return Ok(Candidate::TooLong);
    }
    let mut bytes = Vec::new();
    (&file).take(MAX_LEN).read_to_end(&mut bytes)?;
    Ok(Candidate::Bytes(bytes, file))
}

/// What became of one retired unit's path.
#[derive(Debug, PartialEq, Eq)]
enum Retired {
    Absent,
    Removed,
    /// Left where it is, and why.
    Kept(String),
}

/// What a [`Remover`] did.
#[derive(Debug, PartialEq, Eq)]
enum Removal {
    Removed,
    /// The path no longer names the file that was read: nothing was removed.
    Replaced,
}

/// Removes the file at a path, given the descriptor it was read through.
type Remover<'a> = &'a dyn Fn(&Path, &File) -> io::Result<Removal>;

/// The host's [`Remover`]: unlinks `path` only if it still names the file `held`
/// was opened on, by `lstat` against the descriptor's `fstat` — the same device
/// and inode — and otherwise leaves whatever is there. Reading and removing go
/// by the path separately, so without this a file put at the path in between —
/// a package manager's, an operator's — was removed as if it were the one that
/// had been read. The window that remains is the instant between this `lstat`
/// and the `unlink` that follows it, and a change made to the same file in
/// place; both need root's write access to these directories at that moment.
fn remove_if_unchanged(path: &Path, held: &File) -> io::Result<Removal> {
    let read = held.metadata()?;
    let now = std::fs::symlink_metadata(path)?;
    if (now.dev(), now.ino()) != (read.dev(), read.ino()) {
        return Ok(Removal::Replaced);
    }
    std::fs::remove_file(path)?;
    Ok(Removal::Removed)
}

/// The first directory on the way from `root` to `dir` that is a link, if any,
/// by `lstat` of each in turn: what lies below `root` only, whatever `root`
/// itself is. `O_NOFOLLOW` guards a path's last component alone, so a link in
/// any directory above a unit is followed by the open and by the unlink — what
/// the unlink would remove is then not at the place this module looks at, and
/// may be a unit of setup's own.
fn link_above(root: &Path, dir: &str) -> io::Result<Option<PathBuf>> {
    let mut at = root.to_path_buf();
    for part in dir.split('/') {
        at.push(part);
        if std::fs::symlink_metadata(&at)?.file_type().is_symlink() {
            return Ok(Some(at));
        }
    }
    Ok(None)
}

/// Where an older version may have installed `unit`: below `root`, in `dir`.
fn unit_path(root: &Path, dir: &str, unit: &str) -> PathBuf {
    root.join(dir).join(unit)
}

/// Look where an older version may have installed `unit` — in `dir`, below
/// `root` — and remove it with `remove` if it holds a version this project
/// installed.
fn retire_one(root: &Path, dir: &str, unit: &str, remove: Remover) -> Result<Retired, String> {
    let path = unit_path(root, dir, unit);
    let read = read_candidate(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let (bytes, held) = match read {
        Candidate::Absent => return Ok(Retired::Absent),
        Candidate::NotAFile => return Ok(Retired::Kept("it is not a regular file".into())),
        Candidate::TooLong => {
            return Ok(Retired::Kept(
                "it is longer than any version this project installed".into(),
            ));
        }
        Candidate::Bytes(bytes, held) => (bytes, held),
    };
    if !is_shipped(unit, &bytes) {
        return Ok(Retired::Kept(
            "its content is no version this project installed — edited, or not this project's"
                .into(),
        ));
    }
    if let Some(link) =
        link_above(root, dir).map_err(|e| format!("cannot read {}: {e}", path.display()))?
    {
        return Ok(Retired::Kept(format!(
            "a directory above it is a link ({})",
            link.display()
        )));
    }
    match remove(&path, &held).map_err(|e| format!("cannot remove {}: {e}", path.display()))? {
        Removal::Removed => Ok(Retired::Removed),
        Removal::Replaced => Ok(Retired::Kept(
            "it was replaced while it was being checked".into(),
        )),
    }
}

/// Remove, below `root`, every copy of the four backup units an older
/// version's `cmake --install` left — see the module documentation. Every path
/// is tried; each file removed or kept is said on `say`, and so is each path
/// that could not be read or removed, which also makes this an error.
pub fn remove_retired_units(root: &Path, say: &mut dyn FnMut(String)) -> Result<(), String> {
    remove_retired_units_with(root, say, &remove_if_unchanged)
}

fn remove_retired_units_with(
    root: &Path,
    say: &mut dyn FnMut(String),
    remove: Remover,
) -> Result<(), String> {
    let mut failed = Vec::new();
    for dir in UNIT_DIRS {
        for unit in UNITS {
            let path = unit_path(root, dir, unit);
            match retire_one(root, dir, unit, remove) {
                Ok(Retired::Absent) => {}
                Ok(Retired::Removed) => say(format!(
                    "Removed {}: a backup unit an older version installed. `btrdasd setup` is \
                     now the only writer of the backup units (bd DAS-Backup-Manager-7rf).",
                    path.display()
                )),
                Ok(Retired::Kept(why)) => say(format!(
                    "Kept {}: {why}. Remove it yourself if it is a leftover: it takes effect \
                     whenever setup's {unit} in /etc/systemd/system is gone.",
                    path.display()
                )),
                Err(e) => {
                    say(format!("ERROR: {e}"));
                    failed.push(e);
                }
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} backup unit(s) an older version installed could not be checked or removed: {}",
            failed.len(),
            failed.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `template` as `configure_file(... @ONLY)` writes it for `prefix`.
    fn configured(template: &str, prefix: &str) -> String {
        template.replace(SCRIPT_DIR_PLACEHOLDER, &format!("{prefix}/lib/das-backup"))
    }

    /// Every version of `unit`.
    fn versions(unit: &str) -> Vec<&'static str> {
        SHIPPED
            .iter()
            .filter(|(name, _)| *name == unit)
            .map(|(_, t)| *t)
            .collect()
    }

    /// Write `bytes` at `rel` below `root`.
    fn put(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn run(root: &Path) -> (Result<(), String>, Vec<String>) {
        let mut lines = Vec::new();
        let result = remove_retired_units(root, &mut |l| lines.push(l));
        (result, lines)
    }

    #[test]
    fn each_service_version_has_one_placeholder_on_its_execstart_line_and_timers_none() {
        for (unit, template) in SHIPPED {
            let found: Vec<&str> = template
                .lines()
                .filter(|l| l.contains(SCRIPT_DIR_PLACEHOLDER))
                .collect();
            if unit.ends_with(".timer") {
                assert!(found.is_empty(), "{unit}");
            } else {
                assert_eq!(
                    template.matches(SCRIPT_DIR_PLACEHOLDER).count(),
                    1,
                    "{unit}"
                );
                assert_eq!(found.len(), 1, "{unit}");
                assert!(found[0].starts_with("ExecStart=@DAS_SCRIPT_DIR@/backup-run.sh"));
            }
            assert!(UNITS.contains(&unit), "{unit}");
        }
        // Five versions of each service, one of each timer.
        let counts: Vec<usize> = UNITS.iter().map(|u| versions(u).len()).collect();
        assert_eq!(counts, [5, 5, 1, 1]);
    }

    #[test]
    fn every_version_installed_under_either_prefix_with_any_script_dir_is_removed() {
        // The script directory is whatever the prefix was when CMake was
        // configured, which need not be where the unit landed: a default
        // configure with the GUI put `/usr/local/lib/das-backup` into units
        // installed under `/usr` (bd DAS-Backup-Manager-laj).
        for script_prefix in ["/usr", "/usr/local", "/opt/das"] {
            for dir in UNIT_DIRS {
                for unit in UNITS {
                    for template in versions(unit) {
                        let root = tempfile::tempdir().unwrap();
                        let path = put(
                            root.path(),
                            &format!("{dir}/{unit}"),
                            configured(template, script_prefix).as_bytes(),
                        );
                        let (result, lines) = run(root.path());
                        assert_eq!(result, Ok(()));
                        assert!(!path.exists(), "{dir}/{unit} under {script_prefix}");
                        assert_eq!(
                            lines,
                            [format!(
                                "Removed {}: a backup unit an older version installed. \
                                 `btrdasd setup` is now the only writer of the backup units \
                                 (bd DAS-Backup-Manager-7rf).",
                                path.display()
                            )]
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn all_eight_paths_at_once_are_all_removed() {
        let root = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for dir in UNIT_DIRS {
            for unit in UNITS {
                let latest = *versions(unit).last().unwrap();
                paths.push(put(
                    root.path(),
                    &format!("{dir}/{unit}"),
                    configured(latest, "/usr").as_bytes(),
                ));
            }
        }
        let (result, lines) = run(root.path());
        assert_eq!(result, Ok(()));
        assert_eq!(lines.len(), 8);
        for path in paths {
            assert!(!path.exists(), "{}", path.display());
        }
    }

    #[test]
    fn nothing_there_is_nothing_said() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(run(root.path()), (Ok(()), vec![]));
        // Nor with the directories present and empty.
        for dir in UNIT_DIRS {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        assert_eq!(run(root.path()), (Ok(()), vec![]));
    }

    /// Run on a root holding `bytes` at `usr/lib/systemd/system/<unit>`, and
    /// return what was said; the file must still be there, unchanged.
    fn kept(unit: &str, bytes: &[u8]) -> Vec<String> {
        let root = tempfile::tempdir().unwrap();
        let path = put(
            root.path(),
            &format!("usr/lib/systemd/system/{unit}"),
            bytes,
        );
        let (result, lines) = run(root.path());
        assert_eq!(result, Ok(()));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "{unit} must be left as it was"
        );
        lines
            .into_iter()
            .map(|l| l.replace(&path.display().to_string(), "<path>"))
            .collect()
    }

    const NOT_OURS: &str = "Kept <path>: its content is no version this project installed — \
                            edited, or not this project's. Remove it yourself if it is a \
                            leftover: it takes effect whenever setup's ";

    #[test]
    fn a_unit_that_is_not_byte_for_byte_a_version_is_kept_and_named() {
        let latest = *versions("das-backup.service").last().unwrap();
        let good = configured(latest, "/usr");
        let mut cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("a newline more", format!("{good}\n").into_bytes()),
            (
                "its last byte gone",
                good.as_bytes()[..good.len() - 1].to_vec(),
            ),
            ("the placeholder left in", latest.as_bytes().to_vec()),
            (
                "a relative script dir",
                configured(latest, "usr").into_bytes(),
            ),
            (
                "a space in the script dir",
                configured(latest, "/opt/my das").into_bytes(),
            ),
            (
                "a tab in the script dir",
                configured(latest, "/opt/\tdas").into_bytes(),
            ),
            (
                "another script dir",
                latest
                    .replace(SCRIPT_DIR_PLACEHOLDER, "/usr/lib/das-backup-old")
                    .into_bytes(),
            ),
            (
                "two script dirs",
                latest
                    .replace(
                        SCRIPT_DIR_PLACEHOLDER,
                        "/usr/lib/das-backup\n/usr/lib/das-backup",
                    )
                    .into_bytes(),
            ),
            (
                "the timer's text",
                versions("das-backup.timer")[0].as_bytes().to_vec(),
            ),
            (
                "setup's own unit",
                super::super::templates::render_systemd_service(
                    &super::super::config::Config::default(),
                    false,
                )
                .into_bytes(),
            ),
        ];
        // One byte changed, at the start, inside the head, and in the tail.
        for at in [0, good.find("[Service]").unwrap(), good.len() - 2] {
            let mut b = good.clone().into_bytes();
            b[at] ^= 0x20;
            cases.push(("one byte changed", b));
        }
        for (case, bytes) in cases {
            assert_eq!(
                kept("das-backup.service", &bytes),
                [format!(
                    "{NOT_OURS}das-backup.service in /etc/systemd/system is gone."
                )],
                "{case}"
            );
        }
        // A timer is matched exactly.
        let timer = versions("das-backup-full.timer")[0];
        assert_eq!(
            kept("das-backup-full.timer", format!("{timer} ").as_bytes()),
            [format!(
                "{NOT_OURS}das-backup-full.timer in /etc/systemd/system is gone."
            )]
        );
    }

    /// What `remove_retired_units` says when it keeps `unit` because it is
    /// not a regular file, with its directory written `<dir>`.
    fn not_a_file(unit: &str) -> String {
        format!(
            "Kept <dir>/{unit}: it is not a regular file. Remove it yourself if it is a \
             leftover: it takes effect whenever setup's {unit} in /etc/systemd/system is gone."
        )
    }

    #[test]
    fn a_link_or_a_directory_is_kept_and_what_a_link_names_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("usr/lib/systemd/system");
        std::fs::create_dir_all(&dir).unwrap();
        // A link to a shipped version — what a link names is not ours to judge.
        let target = put(
            root.path(),
            "elsewhere/das-backup.service",
            configured(versions("das-backup.service")[0], "/usr").as_bytes(),
        );
        std::os::unix::fs::symlink(&target, dir.join("das-backup.service")).unwrap();
        std::fs::create_dir(dir.join("das-backup.timer")).unwrap();

        let (result, lines) = run(root.path());
        assert_eq!(result, Ok(()));
        let mut lines: Vec<String> = lines
            .into_iter()
            .map(|l| l.replace(&dir.display().to_string(), "<dir>"))
            .collect();
        lines.sort();
        assert_eq!(
            lines,
            [
                not_a_file("das-backup.service"),
                not_a_file("das-backup.timer")
            ]
        );
        assert!(dir.join("das-backup.service").symlink_metadata().is_ok());
        assert!(target.exists(), "a link's target is never touched");
        assert!(dir.join("das-backup.timer").is_dir());
    }

    #[test]
    fn a_copy_reached_through_a_link_in_a_directory_above_it_is_kept_wherever_the_link_is() {
        // `O_NOFOLLOW` guards a path's last component only, so a link in any
        // directory above the unit was followed by the open and the unlink: the
        // review's R2 had `usr/local/lib/systemd/system` a link into
        // `etc/systemd/system`, and a byte-exact copy there was removed. Every
        // position of the link, on the way to either directory.
        for dir in UNIT_DIRS {
            let parts: Vec<&str> = dir.split('/').collect();
            for i in 0..parts.len() {
                let root = tempfile::tempdir().unwrap();
                // The real tree, away from where the units are looked for.
                let inside = ["real"]
                    .into_iter()
                    .chain(parts[i + 1..].iter().copied())
                    .collect::<Vec<_>>()
                    .join("/");
                let real = put(
                    root.path(),
                    &format!("{inside}/das-backup.timer"),
                    versions("das-backup.timer")[0].as_bytes(),
                );
                let link = root.path().join(parts[..=i].join("/"));
                std::fs::create_dir_all(link.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(root.path().join("real"), &link).unwrap();

                let (result, lines) = run(root.path());
                assert_eq!(result, Ok(()), "{dir}, link at {}", parts[i]);
                let path = root.path().join(dir).join("das-backup.timer");
                assert_eq!(
                    lines,
                    [format!(
                        "Kept {}: a directory above it is a link ({}). Remove it yourself if it \
                         is a leftover: it takes effect whenever setup's das-backup.timer in \
                         /etc/systemd/system is gone.",
                        path.display(),
                        link.display()
                    )],
                    "{dir}, link at {}",
                    parts[i]
                );
                assert!(real.exists(), "what the link leads to is not touched");
            }
        }
    }

    #[test]
    fn a_link_above_nothing_says_nothing_and_the_root_itself_may_be_a_link() {
        // /usr/local is a link on hosts that keep it elsewhere: with no unit
        // behind it there is nothing to keep, and nothing to say.
        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(root.path().join("usr")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.path().join("usr/local")).unwrap();
        assert_eq!(run(root.path()), (Ok(()), vec![]));

        // Only what lies below the root is looked at: a root that is itself
        // reached through a link is an ordinary root.
        let real = tempfile::tempdir().unwrap();
        let path = put(
            real.path(),
            "usr/lib/systemd/system/das-backup.timer",
            versions("das-backup.timer")[0].as_bytes(),
        );
        let outer = tempfile::tempdir().unwrap();
        let via = outer.path().join("root");
        std::os::unix::fs::symlink(real.path(), &via).unwrap();
        let (result, lines) = run(&via);
        assert_eq!(result, Ok(()));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Removed "), "{lines:?}");
        assert!(!path.exists());
    }

    #[test]
    fn a_pipe_is_kept_without_waiting_for_a_writer() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("usr/lib/systemd/system");
        std::fs::create_dir_all(&dir).unwrap();
        let fifo =
            std::ffi::CString::new(dir.join("das-backup-full.timer").to_str().unwrap()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path for the duration of the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);

        // Opened without O_NONBLOCK, a pipe nobody writes to blocks the open
        // for ever: bounded, so that fails this test instead of hanging it.
        let (tx, rx) = std::sync::mpsc::channel();
        let path = root.path().to_path_buf();
        std::thread::spawn(move || {
            tx.send(run(&path)).ok();
        });
        let (result, lines) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the cleanup blocked on a pipe at a retired unit's path");
        assert_eq!(result, Ok(()));
        let lines: Vec<String> = lines
            .into_iter()
            .map(|l| l.replace(&dir.display().to_string(), "<dir>"))
            .collect();
        assert_eq!(lines, [not_a_file("das-backup-full.timer")]);
        assert!(dir.join("das-backup-full.timer").exists());
    }

    #[test]
    fn a_file_past_the_limit_is_not_read_and_one_at_it_is_read_whole() {
        let root = tempfile::tempdir().unwrap();
        let at = put(root.path(), "at", &vec![b'x'; MAX_LEN as usize]);
        let Candidate::Bytes(bytes, _) = read_candidate(&at).unwrap() else {
            panic!("a file at the limit is read");
        };
        assert_eq!(bytes, vec![b'x'; MAX_LEN as usize]);
        let past = put(root.path(), "past", &vec![b'x'; MAX_LEN as usize + 1]);
        assert!(matches!(read_candidate(&past).unwrap(), Candidate::TooLong));

        assert_eq!(
            kept("das-backup.service", &vec![b'x'; MAX_LEN as usize + 1]),
            [
                "Kept <path>: it is longer than any version this project installed. Remove it \
              yourself if it is a leftover: it takes effect whenever setup's das-backup.service \
              in /etc/systemd/system is gone."
            ]
        );
    }

    #[test]
    fn only_the_eight_retired_paths_are_looked_at() {
        let root = tempfile::tempdir().unwrap();
        let shipped = configured(versions("das-backup.service")[0], "/usr");
        // Setup's own units, the scrub, the helper, another prefix, the old
        // fixed location: none of them is a path CMake wrote these units to.
        let others = [
            "etc/systemd/system/das-backup.service",
            "usr/lib/systemd/system/das-scrub.service",
            "usr/lib/systemd/system/btrdasd-helper.service",
            "opt/das/lib/systemd/system/das-backup.service",
            "lib/systemd/system/das-backup.service",
            "usr/lib/systemd/system/das-backup.service.d/override.conf",
        ];
        for rel in others {
            put(root.path(), rel, shipped.as_bytes());
        }
        assert_eq!(run(root.path()), (Ok(()), vec![]));
        for rel in others {
            assert_eq!(
                std::fs::read(root.path().join(rel)).unwrap(),
                shipped.as_bytes(),
                "{rel}"
            );
        }
    }

    #[test]
    fn a_file_that_cannot_be_removed_is_an_error_naming_it_and_the_rest_are_still_tried() {
        let root = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for unit in UNITS {
            paths.push(put(
                root.path(),
                &format!("usr/lib/systemd/system/{unit}"),
                configured(versions(unit)[0], "/usr").as_bytes(),
            ));
        }
        let refuse = &paths[1];
        let remove = |p: &Path, held: &File| {
            if p == refuse {
                Err(io::Error::from_raw_os_error(libc::EROFS))
            } else {
                remove_if_unchanged(p, held)
            }
        };
        let mut lines = Vec::new();
        let result = remove_retired_units_with(root.path(), &mut |l| lines.push(l), &remove);

        let why = format!(
            "cannot remove {}: {}",
            refuse.display(),
            io::Error::from_raw_os_error(libc::EROFS)
        );
        assert_eq!(
            result,
            Err(format!(
                "1 backup unit(s) an older version installed could not be checked or removed: \
                 {why}"
            ))
        );
        assert!(lines.contains(&format!("ERROR: {why}")), "{lines:?}");
        assert_eq!(lines.len(), 4, "three removed, one error: {lines:?}");
        for path in &paths {
            assert_eq!(path.exists(), path == refuse, "{}", path.display());
        }
    }

    #[test]
    fn a_file_swapped_in_after_it_was_judged_ours_is_kept_not_removed() {
        // The file holds a version of ours when it is read, and an edited one
        // takes its place before the removal: the review's R3, through the
        // seam. The host's remover must see that the path no longer names the
        // file that was read, and leave what is there.
        let root = tempfile::tempdir().unwrap();
        let path = put(
            root.path(),
            "usr/lib/systemd/system/das-backup.timer",
            versions("das-backup.timer")[0].as_bytes(),
        );
        let edited = b"[Timer]\n# an operator's own schedule\n".to_vec();
        let swap_then_remove = |p: &Path, held: &File| {
            let new = p.with_file_name("das-backup.timer.new");
            std::fs::write(&new, &edited)?;
            std::fs::rename(&new, p)?;
            remove_if_unchanged(p, held)
        };
        let mut lines = Vec::new();
        let result =
            remove_retired_units_with(root.path(), &mut |l| lines.push(l), &swap_then_remove);

        assert_eq!(result, Ok(()));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            edited,
            "the file that was put there is still there, unchanged"
        );
        let lines: Vec<String> = lines
            .into_iter()
            .map(|l| l.replace(&path.display().to_string(), "<path>"))
            .collect();
        assert_eq!(
            lines,
            [
                "Kept <path>: it was replaced while it was being checked. Remove it yourself \
                 if it is a leftover: it takes effect whenever setup's das-backup.timer in \
                 /etc/systemd/system is gone."
            ]
        );
    }

    /// A file holding `bytes` in a fresh directory, and a descriptor
    /// open on it.
    fn held_file(bytes: &[u8]) -> (tempfile::TempDir, PathBuf, File) {
        let dir = tempfile::tempdir().unwrap();
        let path = put(dir.path(), "unit", bytes);
        let held = File::open(&path).unwrap();
        (dir, path, held)
    }

    #[test]
    fn the_host_remover_removes_the_file_it_holds_a_descriptor_of() {
        let (_dir, path, held) = held_file(b"ours");
        assert_eq!(remove_if_unchanged(&path, &held).unwrap(), Removal::Removed);
        assert!(!path.exists());
    }

    #[test]
    fn the_host_remover_keeps_a_different_file_at_the_path() {
        let (dir, path, held) = held_file(b"ours");
        let other = put(dir.path(), "other", b"someone else's");
        std::fs::rename(&other, &path).unwrap();
        assert_eq!(
            remove_if_unchanged(&path, &held).unwrap(),
            Removal::Replaced
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"someone else's");
    }

    #[test]
    fn the_host_remover_compares_the_path_itself_not_what_a_link_there_leads_to() {
        // A link at the path to a second name of the held file: `stat` on the
        // path answers with the held file's inode, `lstat` with the link's.
        // Only the second is the truth about what unlink would remove.
        let (dir, path, held) = held_file(b"ours");
        let twin = dir.path().join("twin");
        std::fs::hard_link(&path, &twin).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&twin, &path).unwrap();
        assert_eq!(
            remove_if_unchanged(&path, &held).unwrap(),
            Removal::Replaced
        );
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(twin.exists());
    }

    #[test]
    fn the_host_remover_fails_when_nothing_is_at_the_path_any_more() {
        let (_dir, path, held) = held_file(b"ours");
        std::fs::remove_file(&path).unwrap();
        let err = remove_if_unchanged(&path, &held).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_path_that_cannot_be_read_is_an_error_and_every_other_is_still_tried() {
        // A file where a directory should be: opening below it fails with
        // ENOTDIR, which is neither "nothing there" nor "a link" — and fails
        // for root too.
        let root = tempfile::tempdir().unwrap();
        put(root.path(), "usr/lib/systemd", b"not a directory");
        let local = put(
            root.path(),
            "usr/local/lib/systemd/system/das-backup.timer",
            versions("das-backup.timer")[0].as_bytes(),
        );
        let (result, lines) = run(root.path());

        let err = result.unwrap_err();
        assert!(
            err.starts_with(
                "4 backup unit(s) an older version installed could not be checked or removed: \
                 cannot read "
            ),
            "{err}"
        );
        assert_eq!(err.matches("Not a directory").count(), 4, "{err}");
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("ERROR: cannot read "))
                .count(),
            4,
            "{lines:?}"
        );
        assert!(!local.exists(), "the other directory is still cleaned");
        assert_eq!(lines.len(), 5, "{lines:?}");
    }

    #[test]
    fn configured_from_needs_the_head_the_tail_and_a_script_dir_between_them() {
        let t = "A@DAS_SCRIPT_DIR@/backup-run.sh\nZ";
        assert!(configured_from(b"A/usr/lib/das-backup/backup-run.sh\nZ", t));
        assert!(configured_from(b"A//lib/das-backup/backup-run.sh\nZ", t));
        assert!(
            !configured_from(b"B/usr/lib/das-backup/backup-run.sh\nZ", t),
            "head"
        );
        assert!(
            !configured_from(b"A/usr/lib/das-backup/backup-run.sh\nY", t),
            "tail"
        );
        assert!(!configured_from(b"A/backup-run.sh\nZ", t), "no script dir");
        // Shorter than head and tail together, though both are there.
        assert!(!configured_from(b"AZ", "AZ@DAS_SCRIPT_DIR@Z"));
        // No placeholder: exactly the template.
        assert!(configured_from(b"plain\n", "plain\n"));
        assert!(!configured_from(b"plain", "plain\n"));
    }

    #[test]
    fn a_script_dir_is_absolute_ends_in_lib_das_backup_and_has_no_space() {
        assert!(is_script_dir(b"/usr/lib/das-backup"));
        assert!(is_script_dir(b"/lib/das-backup"));
        assert!(!is_script_dir(b"usr/lib/das-backup"));
        assert!(!is_script_dir(b"/usr/lib/das-backup/"));
        assert!(!is_script_dir(b"/usr/lib/das-backups"));
        assert!(!is_script_dir(b"/usr/my lib/das-backup"));
        assert!(!is_script_dir(b"/usr/\x07/lib/das-backup"));
        assert!(!is_script_dir(b""));
    }

    #[test]
    fn a_version_counts_only_for_its_own_unit() {
        let service = configured(versions("das-backup.service")[0], "/usr");
        assert!(is_shipped("das-backup.service", service.as_bytes()));
        assert!(!is_shipped("das-backup-full.service", service.as_bytes()));
        assert!(!is_shipped("das-scrub.service", service.as_bytes()));
        let timer = versions("das-backup.timer")[0];
        assert!(is_shipped("das-backup.timer", timer.as_bytes()));
        assert!(!is_shipped("das-backup-full.timer", timer.as_bytes()));
    }
}
