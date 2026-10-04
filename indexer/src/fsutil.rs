//! Small filesystem and process helpers shared by the modules that change
//! the backup configuration during a run.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

/// Replace `path` with `contents` in one step — [`write_atomic_mode`] with
/// no mode given, so the file keeps the mode it had (and, written by root,
/// its owner and group); a new one gets the creation default.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    write_atomic_mode(path, contents.as_bytes(), None)
}

/// Replace `path` with `contents` in one step: write a new file beside it,
/// flush it to disk, rename it over `path`, then flush the directory. A
/// reader sees the old file or the new one, never part of either — which
/// matters because `config.toml`, `btrbk.conf` and the scripts are replaced
/// while a backup run is using them. A program already reading the old file,
/// as a running bash reads its script, keeps reading it whole: the new one is
/// another inode.
///
/// Before the rename the new file is given its mode — `mode` if given, else
/// the mode of the file it replaces — and, when root writes it, the replaced
/// file's owner and group; so it never stands at `path` with any others. With
/// no mode and nothing to replace it keeps the creation default, `0o666`
/// less the umask. ACLs and other extended attributes are not carried over:
/// the new file has what its directory gives a new file.
///
/// Every call writes a temp file of its own, `.NAME.PID.N.tmp`, created only
/// under a name nothing holds: concurrent writers never touch each other's,
/// and whatever already stands beside `path` — a temp file a crash left, or
/// anything planted — is passed over, never followed, reused or removed. A
/// symlink at `path` is written through, as a plain write would be: the file
/// it names is replaced and the link kept. A link to a file that does not
/// exist fails the write — it fails closed, rather than create a file
/// wherever the link points.
///
/// A failure before the rename leaves the file at `path` untouched and no
/// temp file of this call's behind, and the error names `path`. A failure to
/// flush the directory after the rename is an error too, though the new file
/// is in place by then; it says so.
pub fn write_atomic_mode(path: &Path, contents: &[u8], mode: Option<u32>) -> io::Result<()> {
    write_atomic_with(
        path,
        contents,
        mode,
        &WriteHost {
            // SAFETY: geteuid() has no preconditions and cannot fail.
            euid: unsafe { libc::geteuid() },
            temp_name: &unique_temp_name,
            rename: &|from, to| fs::rename(from, to),
            sync_dir: &|dir| File::open(dir)?.sync_all(),
        },
    )
}

/// What a write asks of the system, so a test can stand in for each part.
struct WriteHost<'a> {
    /// The writer's effective user id: an owner and group are kept only by
    /// root, the one writer that can give a file away.
    euid: u32,
    /// The next name to try for the temp file beside the target.
    temp_name: &'a dyn Fn(&Path) -> PathBuf,
    /// Moves the finished temp file over the target.
    rename: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
    /// Flushes the directory the target is in, so the rename lasts.
    sync_dir: &'a dyn Fn(&Path) -> io::Result<()>,
}

/// How many taken temp-file names a write passes over before it gives up.
const TEMP_TRIES: usize = 64;

/// `.NAME.PID.N.tmp` beside `target`, N counting up across the process: no
/// two calls in a process are offered one name, and no other running process
/// is offered it either.
fn unique_temp_name(target: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(".");
    name.push(target.file_name().unwrap_or_default());
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    target.with_file_name(name)
}

/// [`write_atomic_mode`] on `host`.
fn write_atomic_with(
    path: &Path,
    contents: &[u8],
    mode: Option<u32>,
    host: &WriteHost<'_>,
) -> io::Result<()> {
    let named =
        |e: io::Error| io::Error::new(e.kind(), format!("cannot write {}: {e}", path.display()));
    let target = write_through(path).map_err(named)?;
    let (Some(dir), Some(_)) = (target.parent(), target.file_name()) else {
        return Err(named(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path has no file name",
        )));
    };
    // The file being replaced, if any. Any error but its absence fails the
    // write, though any that could arise here makes the temp file's creation
    // fail too: closed either way, never a guessed mode.
    let old = match fs::metadata(&target) {
        Ok(meta) => Some(meta),
        Err(e) => match e.kind() {
            io::ErrorKind::NotFound => None,
            _ => return Err(named(e)),
        },
    };
    let mode = mode.or(old.as_ref().map(|meta| meta.mode() & 0o7777));
    let owner = old
        .filter(|_| host.euid == 0)
        .map(|meta| (meta.uid(), meta.gid()));
    let (tmp, mut file) = create_temp(&target, mode, host).map_err(named)?;
    let written = (|| {
        if let Some((uid, gid)) = owner {
            std::os::unix::fs::fchown(&file, Some(uid), Some(gid))?;
        }
        file.write_all(contents)?;
        if let Some(mode) = mode {
            // Last before the flush: `open` applied the umask, and a chown or
            // a write by anyone but root clears set-id bits.
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        file.sync_all()?;
        (host.rename)(&tmp, &target)
    })();
    if let Err(e) = written {
        // Best effort, and only this call's own file: the error being
        // returned is the one that matters.
        let _ = fs::remove_file(&tmp);
        return Err(named(e));
    }
    (host.sync_dir)(dir).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "{} was replaced, but its directory could not be flushed to disk: {e}",
                path.display()
            ),
        )
    })
}

/// Create this write's temp file under the first name `host` offers that
/// nothing holds — `create_new`, so a taken name is passed over, never
/// opened. It starts with `mode` less the umask: never wider than it ends.
fn create_temp(
    target: &Path,
    mode: Option<u32>,
    host: &WriteHost<'_>,
) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_TRIES {
        let tmp = (host.temp_name)(target);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode.unwrap_or(0o666))
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free name for a temp file beside it in {TEMP_TRIES} tries"),
    ))
}

/// Where a write to `path` lands: the file a symlink there names — which
/// must exist — or `path` itself, made absolute so its directory has a name.
fn write_through(path: &Path) -> io::Result<PathBuf> {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        fs::canonicalize(path)
    } else {
        std::path::absolute(path)
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
    use std::path::Path;
    use std::process::{Command, ExitStatus, Output};
    use std::sync::Mutex;

    /// Make the file at `path` impossible to replace — for root too — while
    /// it still reads: it moves to the longest name a file can have, which
    /// leaves no room for a temp file's name beside it, and a symlink at
    /// `path` points there. (`setup`'s installer tests, in the binary crate,
    /// carry a copy.)
    pub(crate) fn unreplaceable(path: &Path) {
        let real = path.with_file_name("x".repeat(255));
        std::fs::rename(path, &real).unwrap();
        std::os::unix::fs::symlink(&real, path).unwrap();
    }

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

    // bd DAS-Backup-Manager-6wt fix rounds 4 and 5 — every write replaces
    // the file whole with a new file of its own, which has the old one's
    // mode (and, as root, its owner and group) before it is renamed into
    // place; writers never touch each other's temp files; a failure names
    // the path and leaves the old file.

    use std::io::Read;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

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

    /// Every name in `dir`, sorted.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn real_rename(from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn real_sync_dir(dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }

    fn real_euid() -> u32 {
        // SAFETY: geteuid() has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// The real host, but for the parts a test names.
    fn host<'a>(
        temp_name: &'a dyn Fn(&Path) -> PathBuf,
        rename: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
        sync_dir: &'a dyn Fn(&Path) -> io::Result<()>,
    ) -> WriteHost<'a> {
        WriteHost {
            euid: real_euid(),
            temp_name,
            rename,
            sync_dir,
        }
    }

    /// Offers `names` as temp-file names, in order, and panics if asked for
    /// one more: a test knows exactly which names a write tried.
    fn scripted_names(names: Vec<PathBuf>) -> impl Fn(&Path) -> PathBuf {
        let names = Mutex::new(names.into_iter());
        move |_| {
            names
                .lock()
                .unwrap()
                .next()
                .expect("asked for more temp-file names than scripted")
        }
    }

    /// A group this process may give a file of its own: any, as root; else
    /// one it is a member of besides its own.
    fn another_group() -> u32 {
        if real_euid() == 0 {
            return 4243;
        }
        // SAFETY: getegid() has no preconditions; getgroups() writes at most
        // `len` gids into a buffer that holds `len`.
        let egid = unsafe { libc::getegid() };
        let len = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let mut groups = vec![0 as libc::gid_t; usize::try_from(len).unwrap()];
        let got = unsafe { libc::getgroups(len, groups.as_mut_ptr()) };
        groups.truncate(usize::try_from(got).expect("getgroups"));
        groups
            .into_iter()
            .find(|g| *g != egid)
            .expect("this test needs root, or a user in a second group")
    }

    /// Give `path` an owner and group a write would not give its own new
    /// file: uid 4242 and gid 4243 as root; else this user and a second group
    /// it is in. Returns them.
    fn give_away(path: &Path) -> (u32, u32) {
        let (uid, gid) = if real_euid() == 0 {
            (4242, 4243)
        } else {
            (real_euid(), another_group())
        };
        std::os::unix::fs::chown(path, Some(uid), Some(gid)).unwrap();
        (uid, gid)
    }

    #[test]
    fn write_atomic_refuses_when_the_directory_is_missing_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent/file.txt");
        let err = write_atomic(&path, "x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert!(!path.exists());
        assert!(names_in(dir.path()).is_empty());
    }

    #[test]
    fn a_name_with_no_room_for_a_temp_file_beside_it_fails_and_keeps_the_old_file() {
        // The longest name a file can have: every temp-file name made from it
        // is longer, so the write fails before anything is made — for root
        // too.
        let dir = tempfile::tempdir().unwrap();
        let name = "x".repeat(255);
        let path = dir.path().join(&name);
        fs::write(&path, "old").unwrap();
        let err = write_atomic(&path, "new").unwrap_err();
        assert!(
            err.to_string()
                .starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(names_in(dir.path()), [name]);
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
    fn a_temp_file_that_cannot_be_made_fails_the_write_with_its_own_error_and_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("das-backup.service");
        fs::write(&path, "old unit").unwrap();
        // The one name offered lies in a directory that does not exist.
        let names = scripted_names(vec![dir.path().join("gone/.das-backup.service.tmp")]);
        let err = write_atomic_with(
            &path,
            b"new unit",
            None,
            &host(&names, &real_rename, &real_sync_dir),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert_eq!(
            err.kind(),
            io::ErrorKind::NotFound,
            "the error is what stood in the way: {err}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"old unit");
        assert_eq!(names_in(dir.path()), ["das-backup.service"]);

        // A directory that cannot be written: as root it can, so only then.
        if real_euid() != 0 {
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
            assert_eq!(names_in(&ro), ["btrbk.conf"], "no temp file left");
        }
    }

    #[test]
    fn a_write_that_fails_at_the_rename_removes_its_own_temp_file_and_keeps_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("das-backup.timer");
        fs::write(&path, "old").unwrap();
        let refuse = |_: &Path, _: &Path| -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused"))
        };
        let err = write_atomic_with(
            &path,
            b"new",
            Some(0o644),
            &host(&unique_temp_name, &refuse, &real_sync_dir),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(
            names_in(dir.path()),
            ["das-backup.timer"],
            "its temp file is gone"
        );

        // Through the real rename: a directory at the path.
        let unit = dir.path().join("das-backup.service");
        fs::create_dir(&unit).unwrap();
        fs::write(unit.join("inside"), "kept").unwrap();
        let err = write_atomic_mode(&unit, b"new", Some(0o644)).unwrap_err();
        assert!(
            err.to_string()
                .starts_with(&format!("cannot write {}: ", unit.display())),
            "{err}"
        );
        assert_eq!(fs::read(unit.join("inside")).unwrap(), b"kept");
        assert_eq!(
            names_in(dir.path()),
            ["das-backup.service", "das-backup.timer"]
        );
    }

    #[test]
    fn the_file_gets_exactly_the_mode_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        // 0o664 and 0o666 carry bits a umask of 022 strips: only an
        // explicit chmod after the open leaves them on.
        for mode in [0o755, 0o750, 0o644, 0o600, 0o664, 0o666] {
            let path = dir.path().join(format!("given-{mode:o}"));
            // Over an existing file of another mode: a given mode wins.
            fs::write(&path, "old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            write_atomic_mode(&path, b"x", Some(mode)).unwrap();
            assert_eq!(mode_of(&path), mode, "{mode:o}");
        }
    }

    #[test]
    fn with_no_mode_given_the_new_file_has_the_old_ones_or_the_creation_default() {
        let dir = tempfile::tempdir().unwrap();
        // The reviewer's case first: a private file stays private. Then bits
        // a umask strips, and set-id bits, which a write or a chown clears.
        for mode in [0o600, 0o640, 0o664, 0o666, 0o755, 0o4755, 0o2750] {
            let path = dir.path().join(format!("kept-{mode:o}"));
            fs::write(&path, "old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            write_atomic(&path, "new").unwrap();
            assert_eq!(mode_of(&path), mode, "{mode:o}");
            assert_eq!(fs::read(&path).unwrap(), b"new");
        }
        let path = dir.path().join("new");
        write_atomic(&path, "x").unwrap();
        assert_eq!(mode_of(&path), 0o666 & !umask(), "a new file: the default");
    }

    #[test]
    fn the_new_file_has_its_mode_owner_and_group_before_it_is_renamed_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "old").unwrap();
        let (uid, gid) = give_away(&path);
        // A set-user-id bit: a chown clears it, so a chown made after the
        // chmod would leave the file without it.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o4750)).unwrap();
        let seen = Mutex::new(None);
        let watch = |from: &Path, to: &Path| -> io::Result<()> {
            let meta = fs::metadata(from)?;
            *seen.lock().unwrap() = Some((
                meta.mode() & 0o7777,
                meta.uid(),
                meta.gid(),
                fs::read(from)?,
                fs::read(to)?,
            ));
            fs::rename(from, to)
        };
        let as_root = WriteHost {
            euid: 0,
            temp_name: &unique_temp_name,
            rename: &watch,
            sync_dir: &real_sync_dir,
        };
        write_atomic_with(&path, b"new", None, &as_root).unwrap();
        let (mode, u, g, temp, at_path) = seen.into_inner().unwrap().expect("renamed");
        assert_eq!(mode, 0o4750, "the old file's mode, before the rename");
        assert_eq!((u, g), (uid, gid), "and its owner and group");
        assert_eq!(temp, b"new", "the new file is complete");
        assert_eq!(
            at_path, b"old",
            "while the old one still stands at the path"
        );
        assert_eq!(mode_of(&path), 0o4750);
        assert_eq!(fs::read(&path).unwrap(), b"new");

        // Not root: the mode is kept; the owner and group are the writer's.
        let other = dir.path().join("btrbk.conf");
        fs::write(&other, "old").unwrap();
        give_away(&other);
        fs::set_permissions(&other, fs::Permissions::from_mode(0o640)).unwrap();
        let not_root = WriteHost {
            euid: 4242,
            ..host(&unique_temp_name, &real_rename, &real_sync_dir)
        };
        write_atomic_with(&other, b"new", None, &not_root).unwrap();
        let meta = fs::metadata(&other).unwrap();
        // SAFETY: getegid() has no preconditions and cannot fail.
        let egid = unsafe { libc::getegid() };
        assert_eq!(meta.mode() & 0o7777, 0o640);
        assert_eq!((meta.uid(), meta.gid()), (real_euid(), egid));
    }

    #[test]
    fn the_owner_and_group_are_kept_as_root_and_only_as_root() {
        // Through the real host: whoever runs this test.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "old").unwrap();
        let (uid, gid) = give_away(&path);
        write_atomic(&path, "new").unwrap();
        let meta = fs::metadata(&path).unwrap();
        // SAFETY: getegid() has no preconditions and cannot fail.
        let egid = unsafe { libc::getegid() };
        let expected = if real_euid() == 0 {
            (uid, gid)
        } else {
            (real_euid(), egid)
        };
        assert_eq!((meta.uid(), meta.gid()), expected);
    }

    #[test]
    fn a_taken_temp_name_is_passed_over_and_whatever_holds_it_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("das-backup.timer");
        fs::write(&path, "old").unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "precious").unwrap();
        // Another writer's temp file, a link planted to a file worth keeping,
        // and a directory: each name is taken.
        let theirs = dir.path().join(".das-backup.timer.1.0.tmp");
        fs::write(&theirs, "another writer's").unwrap();
        let link = dir.path().join(".das-backup.timer.1.1.tmp");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        let sub = dir.path().join(".das-backup.timer.1.2.tmp");
        fs::create_dir(&sub).unwrap();
        let free = dir.path().join(".das-backup.timer.1.3.tmp");
        let before = inode(&theirs);
        let names = scripted_names(vec![
            theirs.clone(),
            link.clone(),
            sub.clone(),
            free.clone(),
        ]);

        write_atomic_with(
            &path,
            b"new",
            None,
            &host(&names, &real_rename, &real_sync_dir),
        )
        .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(
            fs::read(&theirs).unwrap(),
            b"another writer's",
            "never truncated, written or removed"
        );
        assert_eq!(inode(&theirs), before);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&victim).unwrap(), b"precious", "never followed");
        assert!(sub.is_dir());
        assert!(
            !free.exists(),
            "the name it took is now the file at the path"
        );
        assert_eq!(
            names_in(dir.path()),
            [
                ".das-backup.timer.1.0.tmp",
                ".das-backup.timer.1.1.tmp",
                ".das-backup.timer.1.2.tmp",
                "das-backup.timer",
                "victim"
            ]
        );
    }

    #[test]
    fn a_write_gives_up_after_so_many_taken_names_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("btrbk.conf");
        fs::write(&path, "old").unwrap();
        let taken: Vec<PathBuf> = (0..TEMP_TRIES)
            .map(|i| dir.path().join(format!(".btrbk.conf.1.{i}.tmp")))
            .collect();
        for name in &taken {
            fs::write(name, "taken").unwrap();
        }
        // One short of the limit: the last name it may try is free.
        let mut offered = taken[..TEMP_TRIES - 1].to_vec();
        offered.push(dir.path().join(".btrbk.conf.2.0.tmp"));
        let names = scripted_names(offered);
        write_atomic_with(
            &path,
            b"new",
            None,
            &host(&names, &real_rename, &real_sync_dir),
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");

        // At the limit: every name it may try is taken.
        let names = scripted_names(taken.clone());
        let err = write_atomic_with(
            &path,
            b"newer",
            None,
            &host(&names, &real_rename, &real_sync_dir),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert!(
            err.to_string()
                .starts_with(&format!("cannot write {}: ", path.display())),
            "{err}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"new");
        for name in &taken {
            assert_eq!(fs::read(name).unwrap(), b"taken", "{}", name.display());
        }
    }

    #[test]
    fn the_directory_is_flushed_after_the_rename_and_a_failure_to_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "old").unwrap();
        let flushed = Mutex::new(Vec::new());
        let refuse = |d: &Path| -> io::Result<()> {
            flushed
                .lock()
                .unwrap()
                .push((d.to_path_buf(), fs::read(&path)?));
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused"))
        };
        let err = write_atomic_with(
            &path,
            b"new",
            None,
            &host(&unique_temp_name, &real_rename, &refuse),
        )
        .unwrap_err();
        assert_eq!(
            *flushed.lock().unwrap(),
            [(dir.path().to_path_buf(), b"new".to_vec())],
            "its own directory, once, after the rename"
        );
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(
            err.to_string()
                .starts_with(&format!("{} was replaced, but ", path.display())),
            "{err}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(names_in(dir.path()), ["config.toml"]);
    }

    #[test]
    fn a_symlink_at_the_path_is_written_through_and_stays_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("btrbk.conf");
        fs::write(&real, "old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
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
        assert_eq!(mode_of(&real), 0o600, "the file it names keeps its mode");
    }

    #[test]
    fn a_symlink_that_names_nothing_fails_the_write_and_nothing_is_made() {
        let dir = tempfile::tempdir().unwrap();
        // One into a directory that is not there; one to a file that is not
        // there, in one that is — a plain write would create that file.
        let into_nothing = dir.path().join("btrbk.conf");
        std::os::unix::fs::symlink(dir.path().join("gone/btrbk.conf"), &into_nothing).unwrap();
        let to_nothing = dir.path().join("config.toml");
        std::os::unix::fs::symlink(dir.path().join("absent.toml"), &to_nothing).unwrap();
        for link in [&into_nothing, &to_nothing] {
            let err = write_atomic(link, "new").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
            assert!(
                err.to_string()
                    .starts_with(&format!("cannot write {}: ", link.display())),
                "{err}"
            );
        }
        assert_eq!(
            names_in(dir.path()),
            ["btrbk.conf", "config.toml"],
            "nothing made, both links kept"
        );
    }

    #[test]
    fn concurrent_writers_each_replace_the_file_whole_and_a_reader_never_sees_part_of_one() {
        // The reviewer's shape: writers replacing one file, a reader checking
        // each read is one whole version.
        const WRITERS: usize = 4;
        const VERSIONS: usize = 100;
        const BODY: usize = 1 << 18;
        /// Version `v` of writer `w`: one byte repeated, then a trailer
        /// naming both.
        fn version(w: usize, v: usize) -> Vec<u8> {
            let mut text = vec![b'a' + u8::try_from(w).unwrap(); BODY];
            text.extend_from_slice(format!("\nEND {w} {v:03}\n").as_bytes());
            text
        }
        /// The writer and version `read` is, if it is one whole version.
        fn whole(read: &[u8]) -> Option<(usize, usize)> {
            let trailer = std::str::from_utf8(read.get(BODY..)?).ok()?;
            let (w, v) = trailer
                .strip_prefix("\nEND ")?
                .strip_suffix('\n')?
                .split_once(' ')?;
            let (w, v): (usize, usize) = (w.parse().ok()?, v.parse().ok()?);
            (w <= WRITERS && v <= VERSIONS && read == version(w, v)).then_some((w, v))
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, version(WRITERS, 0)).unwrap();
        let done = AtomicBool::new(false);
        let (reads, torn) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let errors = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            s.spawn(|| {
                while !done.load(Ordering::SeqCst) {
                    match fs::read(&path) {
                        Ok(read) => {
                            reads.fetch_add(1, Ordering::SeqCst);
                            if whole(&read).is_none() {
                                torn.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        Err(e) => errors.lock().unwrap().push(format!("read: {e}")),
                    }
                }
            });
            let writers: Vec<_> = (0..WRITERS)
                .map(|w| {
                    let (path, errors) = (&path, &errors);
                    s.spawn(move || {
                        for v in 1..=VERSIONS {
                            if let Err(e) = write_atomic_mode(path, &version(w, v), None) {
                                errors.lock().unwrap().push(format!("write {w} {v}: {e}"));
                            }
                        }
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap();
            }
            done.store(true, Ordering::SeqCst);
        });
        let (reads, torn) = (reads.into_inner(), torn.into_inner());
        let errors = errors.into_inner().unwrap();
        // Shown with --nocapture: how much the reader saw.
        eprintln!(
            "{reads} reads during {} writes; {torn} torn; {} writes failed",
            WRITERS * VERSIONS,
            errors.len()
        );
        assert!(
            errors.is_empty() && torn == 0,
            "{} of {} writes failed, as {:?}; {torn} of {reads} reads were not one whole version",
            errors.len(),
            WRITERS * VERSIONS,
            &errors[..errors.len().min(2)]
        );
        let last = whole(&fs::read(&path).unwrap()).expect("the file ends whole");
        assert_eq!(
            last.1, VERSIONS,
            "and is one writer's last version: {last:?}"
        );
        assert_eq!(
            names_in(dir.path()),
            ["config.toml"],
            "no temp file left behind"
        );
    }
}
