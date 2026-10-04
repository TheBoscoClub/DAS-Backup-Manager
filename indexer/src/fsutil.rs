//! Small filesystem and process helpers shared by the modules that change
//! the backup configuration during a run.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Replace `path` with `contents` in one step: write a sibling temp file,
/// flush it to disk, then rename it over the target. A reader sees the old
/// file or the new one, never a truncated one — which matters because
/// `config.toml` and `btrbk.conf` are rewritten while a backup run is using
/// them. The new file gets the creation default mode; see
/// [`write_atomic_mode`], which this is with no mode given.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    write_atomic_mode(path, contents.as_bytes(), None)
}

/// [`write_atomic`] for bytes, the new file given `mode` — its permission
/// bits — before a byte is written to it, so it never stands at `path` with
/// any other. `None` keeps the creation default, `0o666` less the umask.
///
/// A program already reading the old file, as a running bash reads its
/// script, keeps reading the old file whole: the new one is another inode,
/// renamed over the name. A symlink at `path` is written through, as a plain
/// write would be — its target is replaced and the link kept, so a file that
/// lives elsewhere and is linked here is not split in two. Whatever stands
/// where the temp file goes is removed, never followed or reused. On failure
/// the file at `path` is untouched, no temp file is left, and the error names
/// `path`.
pub fn write_atomic_mode(path: &Path, contents: &[u8], mode: Option<u32>) -> io::Result<()> {
    let named =
        |e: io::Error| io::Error::new(e.kind(), format!("cannot write {}: {e}", path.display()));
    let target = write_through(path).map_err(named)?;
    let name = target.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        named(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path has no file name",
        ))
    })?;
    let tmp = target.with_file_name(format!(".{name}.tmp"));
    let result = (|| {
        // A temp file a crash left — or anything planted under its name —
        // goes first; the new one is then created fresh (O_EXCL), so nothing
        // already there is written through.
        match fs::remove_file(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode.unwrap_or(0o666))
            .open(&tmp)?;
        if let Some(mode) = mode {
            // `open` applied the umask: set exactly `mode`.
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        // Best effort: the error being returned is the one that matters, and
        // a temp file that was never made, or was renamed, is no loss.
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(named)
}

/// Where a plain write to `path` would land: the file a symlink there names,
/// or `path` itself.
fn write_through(path: &Path) -> io::Result<PathBuf> {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        fs::canonicalize(path)
    } else {
        Ok(path.to_path_buf())
    }
}

/// Every external command `adopt` and `expire` run goes through this, so
/// tests can script what `btrfs`, `findmnt` and `blkid` answer.
pub trait CommandRunner {
    fn output(&self, cmd: &mut Command) -> io::Result<Output>;
}

/// The real thing.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn output(&self, cmd: &mut Command) -> io::Result<Output> {
        cmd.output()
    }
}

/// A scripted `CommandRunner` for tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::CommandRunner;
    use std::io;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, ExitStatus, Output};
    use std::sync::Mutex;

    /// Answers by the command's full argv, joined with single spaces. An
    /// unscripted command exits 1 with no output, so a test cannot pass by
    /// accident on a call it never expected.
    pub(crate) struct Scripted {
        answers: Vec<(String, i32, String)>,
        /// Paths whose `btrfs subvolume delete` really removes the directory.
        deletable: Vec<String>,
        calls: Mutex<Vec<String>>,
    }

    impl Scripted {
        pub(crate) fn new(answers: &[(&str, i32, &str)]) -> Self {
            Self::from_owned(
                answers
                    .iter()
                    .map(|(k, c, o)| (k.to_string(), *c, o.to_string()))
                    .collect(),
            )
        }

        pub(crate) fn from_owned(answers: Vec<(String, i32, String)>) -> Self {
            Self {
                answers,
                deletable: Vec::new(),
                calls: Mutex::new(Vec::new()),
            }
        }

        /// A runner where `btrfs subvolume delete <path>` for each listed path
        /// removes that directory and exits 0, so a test can assert on what
        /// remains. Every other command is unscripted (exit 1).
        pub(crate) fn deleting(paths: Vec<String>) -> Self {
            Self {
                deletable: paths,
                ..Self::from_owned(Vec::new())
            }
        }

        /// Every argv run so far, in order.
        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for Scripted {
        fn output(&self, cmd: &mut Command) -> io::Result<Output> {
            let argv = std::iter::once(cmd.get_program())
                .chain(cmd.get_args())
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ");
            self.calls.lock().unwrap().push(argv.clone());
            if let Some(path) = argv
                .strip_prefix("btrfs subvolume delete ")
                .filter(|p| self.deletable.iter().any(|d| d == p))
            {
                std::fs::remove_dir_all(path)?;
                return Ok(Output {
                    status: ExitStatus::from_raw(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            let (code, stdout) = self
                .answers
                .iter()
                .find(|(k, _, _)| *k == argv)
                .map(|(_, c, o)| (*c, o.clone()))
                .unwrap_or((1, String::new()));
            Ok(Output {
                status: ExitStatus::from_raw(code << 8),
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_runner_captures_output_and_exit_status() {
        let out = SystemRunner
            .output(std::process::Command::new("sh").args(["-c", "printf hi; exit 3"]))
            .unwrap();
        assert_eq!(out.stdout, b"hi");
        assert_eq!(out.status.code(), Some(3));
        assert!(
            SystemRunner
                .output(&mut std::process::Command::new(
                    "/nonexistent/das-no-such-binary"
                ))
                .is_err()
        );
    }

    #[test]
    fn write_atomic_refuses_when_the_directory_is_missing_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent/file.txt");
        assert!(write_atomic(&path, "x").is_err());
        assert!(!path.exists());
    }

    #[test]
    fn write_atomic_keeps_the_old_file_when_the_temp_file_cannot_be_created() {
        // A directory where the temp file would go makes creation fail.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, "old").unwrap();
        std::fs::create_dir(dir.path().join(".f.tmp")).unwrap();
        assert!(write_atomic(&path, "new").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }

    // bd DAS-Backup-Manager-6wt fix round 4 — every file setup writes is
    // replaced whole, keeps its mode, and a failure names it.

    use std::fs::File;
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

    fn inode(path: &Path) -> u64 {
        fs::metadata(path).unwrap().ino()
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// This process's umask, as the kernel reports it. Read, never set: tests
    /// run on threads that share it.
    fn umask() -> u32 {
        let status = fs::read_to_string("/proc/self/status").unwrap();
        let field = status
            .lines()
            .find_map(|l| l.strip_prefix("Umask:"))
            .expect("Umask: in /proc/self/status");
        u32::from_str_radix(field.trim(), 8).unwrap()
    }

    #[test]
    fn a_reader_of_the_old_file_reads_it_whole_and_a_new_open_reads_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup-run.sh");
        // Larger than any read buffer, so the reader is part-way in, as a
        // running bash is.
        let old: Vec<u8> = (0..20_000u32)
            .flat_map(|i| format!("# old line {i}\n").into_bytes())
            .collect();
        fs::write(&path, &old).unwrap();
        let before = inode(&path);
        let mut reader = File::open(&path).unwrap();
        let mut first = vec![0u8; 4096];
        reader.read_exact(&mut first).unwrap();

        write_atomic_mode(&path, b"#!/bin/bash\nnew\n", Some(0o755)).unwrap();

        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(
            [first, rest].concat(),
            old,
            "a file already open is the old one, whole"
        );
        assert_eq!(fs::read(&path).unwrap(), b"#!/bin/bash\nnew\n");
        assert_ne!(inode(&path), before, "a new file, renamed into place");
    }

    #[test]
    fn a_write_that_fails_leaves_the_old_file_byte_identical_and_names_it() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the temp file goes: creation fails, for root too.
        let path = dir.path().join("das-backup.service");
        fs::write(&path, "old unit").unwrap();
        fs::create_dir(dir.path().join(".das-backup.service.tmp")).unwrap();
        let err = write_atomic_mode(&path, b"new unit", None).unwrap_err();
        assert!(
            err.to_string()
                .starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert_eq!(
            err.kind(),
            io::ErrorKind::IsADirectory,
            "the error is what stands in the way, not a consequence of it: {err}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"old unit");

        // A directory that cannot be written: as root it can, so only then.
        if unsafe { libc::geteuid() } != 0 {
            let ro = dir.path().join("ro");
            fs::create_dir(&ro).unwrap();
            let file = ro.join("btrbk.conf");
            fs::write(&file, "old conf").unwrap();
            fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
            let err = write_atomic_mode(&file, b"new conf", None).unwrap_err();
            fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(
                err.to_string()
                    .starts_with(&format!("cannot write {}: ", file.display())),
                "{err}"
            );
            assert_eq!(fs::read(&file).unwrap(), b"old conf");
            assert_eq!(fs::read_dir(&ro).unwrap().count(), 1, "no temp file left");
        }
    }

    #[test]
    fn a_write_that_fails_after_its_temp_file_was_made_removes_it() {
        // A directory at the path: the temp file is made and written, and
        // only the rename fails.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("das-backup.timer");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("inside"), "kept").unwrap();
        let err = write_atomic_mode(&path, b"new", Some(0o644)).unwrap_err();
        assert!(
            err.to_string()
                .starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert!(
            !dir.path().join(".das-backup.timer.tmp").exists(),
            "the temp file is removed"
        );
        assert_eq!(fs::read(path.join("inside")).unwrap(), b"kept");
    }

    #[test]
    fn the_file_gets_exactly_the_mode_it_is_given_or_the_creation_default() {
        let dir = tempfile::tempdir().unwrap();
        // 0o664 and 0o666 carry bits a umask of 022 strips: only an
        // explicit chmod after the open leaves them on.
        for mode in [0o755, 0o750, 0o644, 0o600, 0o664, 0o666] {
            let path = dir.path().join(format!("given-{mode:o}"));
            // Over an existing file of another mode, too: the old mode must
            // not carry over.
            fs::write(&path, "old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            write_atomic_mode(&path, b"x", Some(mode)).unwrap();
            assert_eq!(mode_of(&path), mode, "{mode:o}");
        }
        let path = dir.path().join("default");
        write_atomic_mode(&path, b"x", None).unwrap();
        assert_eq!(mode_of(&path), 0o666 & !umask());
    }

    #[test]
    fn a_symlink_at_the_path_is_written_through_and_stays_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("btrbk.conf");
        fs::write(&real, "old").unwrap();
        let link = dir.path().join("link.conf");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomic_mode(&link, b"new", None).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link is kept: one real file, as a plain write leaves it"
        );
        assert_eq!(fs::read(&real).unwrap(), b"new");
    }

    #[test]
    fn nothing_planted_where_the_temp_file_goes_is_ever_followed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("das-backup.timer");
        fs::write(&path, "old").unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "precious").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join(".das-backup.timer.tmp")).unwrap();

        write_atomic_mode(&path, b"new", None).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(
            fs::symlink_metadata(&path).unwrap().file_type().is_file(),
            "the path holds the new file, not the planted link"
        );
    }
}
