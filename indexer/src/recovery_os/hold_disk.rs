//! `btrdasd recovery-os hold-disk`: hold a whole recovery disk open with
//! `O_EXCL` for as long as the recovery-os-updater VM may be using it
//! (bd DAS-Backup-Manager-7wb).
//!
//! A recovery drive's partition 2 holds both its own OS and the backups the
//! host receives into it. While `scripts/recovery-os-vm.sh` has that drive
//! booted in a VM, the guest kernel has the filesystem mounted read-write; a
//! host mount of it at the same time — a second kernel — corrupts it. The
//! maintenance lock keeps this project's own jobs away. This is the guard
//! that does not depend on anyone honouring a lock:
//!
//! - An `O_EXCL` open of a block device is an exclusive *claim* on it. While
//!   the whole disk is claimed, the kernel refuses to claim any of its
//!   partitions, and every mount claims its device — so mounting partition 1
//!   or 2, by device or by UUID, in any mount namespace, fails with "already
//!   mounted or mount point busy".
//! - qemu opens the disk without `O_EXCL`, which a claim does not prevent.
//! - By the same claim rules the reverse holds: a mounted partition marks its
//!   whole disk claimed, so a disk with a mounted partition cannot be claimed
//!   (`EBUSY`), and a hold that succeeds shows nothing on the host has a
//!   partition of it mounted.
//!
//! Proven 2026-10-03 on a test VM with a partitioned loop device (bd
//! DAS-Backup-Manager-frb): baseline mount worked; while held, a non-exclusive
//! `O_RDWR` open of the whole disk worked and the partition mount was refused
//! by device and by UUID; after release the mount worked again. The reverse
//! direction is read off the kernel's `bd_may_claim`, not part of that proof.
//!
//! The open is read-only — the holder never writes. The process announces
//! the hold with one line, `held <path> pid <pid>`, then sleeps in `sigwait`
//! until SIGTERM, SIGINT or SIGHUP. It never reads stdin and never writes
//! stdout again, so a driver that dies — closing both pipes — cannot end the
//! hold: only a signal (or `SIGKILL`, when the kernel drops the claim) can.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

/// The signals that end a hold. Anything else that ends the process ends the
/// claim too (the kernel drops it with the descriptor), just less tidily.
pub const TERMINATION_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// Why a disk could not be held. Every one is a refusal: exit 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldError {
    /// The path could not even be looked at.
    Stat { path: String, detail: String },
    /// Not a block device. Never opened: an open has side effects on some
    /// files (a FIFO with no writer would block forever).
    NotBlockDevice { path: String, kind: &'static str },
    /// The kernel refused the claim: a partition of it is mounted somewhere,
    /// or another program holds it exclusively.
    Busy { path: String },
    /// Any other reason the open failed.
    Open { path: String, detail: String },
    /// What was opened is not the block device that was checked (the path
    /// was replaced in between — a USB disk re-enumerating, say).
    Changed { path: String },
}

impl std::fmt::Display for HoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HoldError::Stat { path, detail } => write!(f, "cannot examine {path}: {detail}"),
            HoldError::NotBlockDevice { path, kind } => {
                write!(f, "{path} is {kind}, not a block device")
            }
            HoldError::Busy { path } => {
                write!(f, "{path} is in use — mounted or held by another program")
            }
            HoldError::Open { path, detail } => write!(f, "cannot open {path}: {detail}"),
            HoldError::Changed { path } => write!(
                f,
                "{path} changed while it was being opened — not the device that was checked"
            ),
        }
    }
}

/// What kind of file a non-block path is, for the refusal. A symlink cannot
/// reach here: the metadata is read through it.
fn file_kind(ft: &std::fs::FileType) -> &'static str {
    if ft.is_dir() {
        "a directory"
    } else if ft.is_char_device() {
        "a character device"
    } else if ft.is_fifo() {
        "a FIFO"
    } else if ft.is_socket() {
        "a socket"
    } else if ft.is_file() {
        "a regular file"
    } else {
        "an unknown kind of file"
    }
}

/// The device number of a block device, or the refusal for anything else.
fn block_device_rdev(path: &Path, meta: &std::fs::Metadata) -> Result<u64, HoldError> {
    let ft = meta.file_type();
    if !ft.is_block_device() {
        return Err(HoldError::NotBlockDevice {
            path: path.display().to_string(),
            kind: file_kind(&ft),
        });
    }
    Ok(meta.rdev())
}

/// `EBUSY` is the kernel refusing the claim; anything else is said as it is.
fn open_error(path: &Path, e: &std::io::Error) -> HoldError {
    if e.raw_os_error() == Some(libc::EBUSY) {
        HoldError::Busy {
            path: path.display().to_string(),
        }
    } else {
        HoldError::Open {
            path: path.display().to_string(),
            detail: e.to_string(),
        }
    }
}

/// The open file must be the block device that was checked before opening.
fn same_device(
    path: &Path,
    checked_rdev: u64,
    opened_is_block: bool,
    opened_rdev: u64,
) -> Result<(), HoldError> {
    if opened_is_block && opened_rdev == checked_rdev {
        Ok(())
    } else {
        Err(HoldError::Changed {
            path: path.display().to_string(),
        })
    }
}

/// Open `path` — a whole disk, usually a `/dev/disk/by-id` symlink — read-only
/// with `O_EXCL`: the exclusive claim described in the module documentation.
/// `std` adds `O_CLOEXEC` to every open, so nothing this process might start
/// would inherit the claim.
///
/// This binds the checks to the host and holds no decision of its own: what
/// is refused, and how an open error reads, are [`block_device_rdev`],
/// [`open_error`] and [`same_device`].
pub fn open_exclusive(path: &Path) -> Result<File, HoldError> {
    let stat_error = |e: std::io::Error| HoldError::Stat {
        path: path.display().to_string(),
        detail: e.to_string(),
    };
    let checked = block_device_rdev(path, &std::fs::metadata(path).map_err(stat_error)?)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_EXCL)
        .open(path)
        .map_err(|e| open_error(path, &e))?;
    let opened = file.metadata().map_err(stat_error)?;
    same_device(
        path,
        checked,
        opened.file_type().is_block_device(),
        opened.rdev(),
    )?;
    Ok(file)
}

/// Write `message` as one whole line in a single `write_all`: stderr is
/// unbuffered, and `writeln!` would issue one write per fragment, which
/// another writer to the same file could interleave with. A message that
/// cannot be written changes nothing about the hold, so its error is dropped.
fn say(err: &mut dyn Write, message: &str) {
    let _ = err.write_all(format!("{message}\n").as_bytes());
}

/// How a signal number reads in the log.
fn signal_name(signal: libc::c_int) -> String {
    match signal {
        libc::SIGTERM => "SIGTERM".to_string(),
        libc::SIGINT => "SIGINT".to_string(),
        libc::SIGHUP => "SIGHUP".to_string(),
        other => format!("signal {other}"),
    }
}

/// The hold, with the host taken out: `open` claims the device (its result
/// is held until the end, then dropped — the close), `wait` blocks until a
/// termination signal. Returns the exit code: 0 released on a signal, 2
/// refused or failed.
///
/// The line is written in one piece and flushed before waiting, so whoever
/// reads it knows the claim is in place. If it cannot be written the hold is
/// let go at once — a claim nobody knows about protects nothing and would
/// keep the disk from the host for no reason.
pub fn hold_with<H>(
    device: &Path,
    pid: u32,
    open: impl FnOnce(&Path) -> Result<H, HoldError>,
    wait: impl FnOnce() -> std::io::Result<libc::c_int>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let shown = device.display();
    let held = match open(device) {
        Ok(held) => held,
        Err(e) => {
            say(err, &format!("Error: cannot hold {shown}: {e}"));
            return 2;
        }
    };
    let line = format!("held {shown} pid {pid}\n");
    if let Err(e) = out.write_all(line.as_bytes()).and_then(|()| out.flush()) {
        drop(held);
        say(
            err,
            &format!("Error: could not announce the hold on {shown}: {e} — released"),
        );
        return 2;
    }
    let signal = wait();
    drop(held);
    match signal {
        Ok(signal) => {
            say(err, &format!("released {shown} on {}", signal_name(signal)));
            0
        }
        Err(e) => {
            say(
                err,
                &format!("Error: waiting for a termination signal failed: {e} — released {shown}"),
            );
            2
        }
    }
}

/// [`TERMINATION_SIGNALS`], blocked in the calling thread so that they queue
/// for [`TerminationSignals::wait`] instead of killing the process. Block them
/// before opening the disk: a SIGTERM that arrives between the announcement
/// and the wait then still ends the hold with exit 0.
///
/// Blocking applies to the calling thread and to threads it starts later.
/// `btrdasd` is single-threaded when it gets here, so no other thread can
/// receive these signals with their default action (terminate).
pub struct TerminationSignals {
    set: libc::sigset_t,
}

impl TerminationSignals {
    /// Block the termination signals in this thread, then give them back
    /// their default disposition. A parent may have left one ignored — bash
    /// starts every background job with SIGINT ignored — and an ignored
    /// signal may be discarded instead of queued. Blocked first, so the
    /// default action (terminate) can never fire in between.
    pub fn block() -> std::io::Result<Self> {
        // SAFETY: `sigset_t` is plain data; `sigemptyset` initialises it.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: `set` is a valid, initialised set and every signal number is
        // a valid constant, so neither call can fail.
        unsafe {
            libc::sigemptyset(&mut set);
            for signal in TERMINATION_SIGNALS {
                libc::sigaddset(&mut set, signal);
            }
        }
        // SAFETY: `set` is valid; the old mask is not wanted.
        let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc));
        }
        for signal in TERMINATION_SIGNALS {
            // SAFETY: SIG_DFL is a valid disposition for these signals, and
            // they are blocked, so it cannot act before `wait` takes them.
            if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(TerminationSignals { set })
    }

    /// Wait for one of them and return its number.
    pub fn wait(&self) -> std::io::Result<libc::c_int> {
        let mut signal: libc::c_int = 0;
        // SAFETY: `self.set` is valid and `signal` is a valid out-pointer.
        let rc = unsafe { libc::sigwait(&self.set, &mut signal) };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc));
        }
        Ok(signal)
    }
}

/// `btrdasd recovery-os hold-disk --device <path>`: exit 0 once released by a
/// termination signal, 2 when refused. The signals are blocked first, then
/// the disk is claimed, then the hold is announced on stdout.
///
/// To outlive a terminal or a dying parent, start it in a session of its own
/// (`setsid`), as `recovery-os-vm.sh` does: otherwise Ctrl-C or a hangup in
/// that terminal reaches this process too and ends the hold.
pub fn run(device: &Path) -> i32 {
    let signals = match TerminationSignals::block() {
        Ok(signals) => signals,
        Err(e) => {
            eprintln!("Error: cannot block the termination signals: {e}");
            return 2;
        }
    };
    hold_with(
        device,
        std::process::id(),
        open_exclusive,
        || signals.wait(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;

    const DISK: &str = "/dev/disk/by-id/ata-ST2000DM008-2FR102_ZK208Q77";

    // ----- refusals before anything is opened ------------------------------

    #[test]
    fn a_regular_file_is_refused_as_not_a_block_device() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("disk.img");
        std::fs::write(&img, b"not a disk").unwrap();
        let err = open_exclusive(&img).unwrap_err();
        let path = img.display().to_string();
        assert_eq!(
            err,
            HoldError::NotBlockDevice {
                path: path.clone(),
                kind: "a regular file"
            }
        );
        assert_eq!(
            err.to_string(),
            format!("{path} is a regular file, not a block device")
        );
    }

    #[test]
    fn a_directory_is_refused_as_not_a_block_device() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_exclusive(dir.path()).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "{} is a directory, not a block device",
                dir.path().display()
            )
        );
    }

    #[test]
    fn a_character_device_is_refused_as_not_a_block_device() {
        let err = open_exclusive(Path::new("/dev/null")).unwrap_err();
        assert_eq!(
            err,
            HoldError::NotBlockDevice {
                path: "/dev/null".into(),
                kind: "a character device"
            }
        );
    }

    /// A by-id path is a symlink, so the check must judge what it points at:
    /// a link to a regular file is still a regular file.
    #[test]
    fn a_symlink_is_judged_by_what_it_points_at() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("disk.img");
        std::fs::write(&img, b"x").unwrap();
        let link = dir.path().join("ata-MODEL_SERIAL");
        std::os::unix::fs::symlink(&img, &link).unwrap();
        assert_eq!(
            open_exclusive(&link).unwrap_err(),
            HoldError::NotBlockDevice {
                path: link.display().to_string(),
                kind: "a regular file"
            }
        );
    }

    #[test]
    fn a_missing_path_is_refused_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("ata-GONE_SERIAL");
        let err = open_exclusive(&gone).unwrap_err();
        let path = gone.display().to_string();
        assert_eq!(
            err,
            HoldError::Stat {
                path: path.clone(),
                detail: "No such file or directory (os error 2)".into()
            }
        );
        assert_eq!(
            err.to_string(),
            format!("cannot examine {path}: No such file or directory (os error 2)")
        );
    }

    /// FIFOs and sockets are classified without ever being opened: an
    /// `O_RDONLY` open of a FIFO with no writer would block forever.
    #[test]
    fn fifos_and_sockets_are_named_and_never_opened() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let meta = std::fs::metadata(&fifo).unwrap();
        assert_eq!(
            block_device_rdev(&fifo, &meta).unwrap_err().to_string(),
            format!("{} is a FIFO, not a block device", fifo.display())
        );
        let sock = dir.path().join("sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let meta = std::fs::metadata(&sock).unwrap();
        assert_eq!(
            block_device_rdev(&sock, &meta).unwrap_err().to_string(),
            format!("{} is a socket, not a block device", sock.display())
        );
    }

    // ----- why an open failed ------------------------------------------------

    #[test]
    fn ebusy_means_mounted_or_held_by_another_program() {
        let e = io::Error::from_raw_os_error(libc::EBUSY);
        let err = open_error(Path::new(DISK), &e);
        assert_eq!(err, HoldError::Busy { path: DISK.into() });
        assert_eq!(
            err.to_string(),
            format!("{DISK} is in use — mounted or held by another program")
        );
    }

    #[test]
    fn any_other_open_error_is_reported_as_it_is() {
        let e = io::Error::from_raw_os_error(libc::EACCES);
        let err = open_error(Path::new(DISK), &e);
        assert_eq!(
            err,
            HoldError::Open {
                path: DISK.into(),
                detail: "Permission denied (os error 13)".into()
            }
        );
        assert_eq!(
            err.to_string(),
            format!("cannot open {DISK}: Permission denied (os error 13)")
        );
    }

    // ----- the opened file is the device that was checked --------------------

    #[test]
    fn the_opened_file_must_be_the_block_device_that_was_checked() {
        let p = Path::new(DISK);
        assert_eq!(same_device(p, 0x0810, true, 0x0810), Ok(()));
        let changed = HoldError::Changed { path: DISK.into() };
        assert_eq!(same_device(p, 0x0810, true, 0x0820), Err(changed.clone()));
        assert_eq!(same_device(p, 0x0810, false, 0x0810), Err(changed.clone()));
        assert_eq!(
            changed.to_string(),
            format!("{DISK} changed while it was being opened — not the device that was checked")
        );
    }

    // ----- the hold ------------------------------------------------------------

    /// Records, in one ordered log, what the hold did: opened, wrote, flushed,
    /// waited, closed.
    #[derive(Clone, Default)]
    struct Log(Rc<RefCell<Vec<String>>>);

    impl Log {
        fn push(&self, s: impl Into<String>) {
            self.0.borrow_mut().push(s.into());
        }
        fn entries(&self) -> Vec<String> {
            self.0.borrow().clone()
        }
    }

    /// Stands in for the held disk: its drop is the close.
    struct Held(Log);

    impl Drop for Held {
        fn drop(&mut self) {
            self.0.push("closed");
        }
    }

    /// A writer that logs, or fails on write or flush.
    struct Recorder {
        log: Log,
        name: &'static str,
        fail_write: bool,
        fail_flush: bool,
    }

    impl Recorder {
        fn new(log: &Log, name: &'static str) -> Self {
            Recorder {
                log: log.clone(),
                name,
                fail_write: false,
                fail_flush: false,
            }
        }
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                return Err(io::Error::from_raw_os_error(libc::EPIPE));
            }
            self.log
                .push(format!("{}: {}", self.name, String::from_utf8_lossy(buf)));
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                return Err(io::Error::from_raw_os_error(libc::EPIPE));
            }
            self.log.push(format!("{} flushed", self.name));
            Ok(())
        }
    }

    fn opener(log: &Log) -> impl FnOnce(&Path) -> Result<Held, HoldError> {
        let log = log.clone();
        move |p: &Path| {
            log.push(format!("open {}", p.display()));
            Ok(Held(log.clone()))
        }
    }

    fn waiter(
        log: &Log,
        answer: io::Result<libc::c_int>,
    ) -> impl FnOnce() -> io::Result<libc::c_int> {
        let log = log.clone();
        move || {
            log.push("wait");
            answer
        }
    }

    #[test]
    fn a_hold_announces_one_line_waits_holding_the_disk_then_closes_it() {
        let log = Log::default();
        let mut out = Recorder::new(&log, "out");
        let mut err = Recorder::new(&log, "err");
        let code = hold_with(
            Path::new(DISK),
            4242,
            opener(&log),
            waiter(&log, Ok(libc::SIGTERM)),
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        assert_eq!(
            log.entries(),
            [
                format!("open {DISK}"),
                format!("out: held {DISK} pid 4242\n"),
                "out flushed".to_string(),
                // still held while waiting: "closed" comes after "wait"
                "wait".to_string(),
                "closed".to_string(),
                format!("err: released {DISK} on SIGTERM\n"),
            ]
        );
    }

    #[test]
    fn sigint_and_sighup_end_a_hold_too_and_are_named() {
        for (signal, name) in [(libc::SIGINT, "SIGINT"), (libc::SIGHUP, "SIGHUP")] {
            let log = Log::default();
            let code = hold_with(
                Path::new(DISK),
                7,
                opener(&log),
                waiter(&log, Ok(signal)),
                &mut Recorder::new(&log, "out"),
                &mut Recorder::new(&log, "err"),
            );
            assert_eq!(code, 0);
            assert_eq!(
                log.entries().last().unwrap(),
                &format!("err: released {DISK} on {name}\n")
            );
        }
        assert_eq!(
            signal_name(libc::SIGUSR1),
            format!("signal {}", libc::SIGUSR1)
        );
    }

    #[test]
    fn a_refused_open_announces_nothing_and_never_waits() {
        let log = Log::default();
        let busy =
            |_: &Path| -> Result<Held, HoldError> { Err(HoldError::Busy { path: DISK.into() }) };
        let code = hold_with(
            Path::new(DISK),
            1,
            busy,
            waiter(&log, Ok(libc::SIGTERM)),
            &mut Recorder::new(&log, "out"),
            &mut Recorder::new(&log, "err"),
        );
        assert_eq!(code, 2);
        assert_eq!(
            log.entries(),
            [format!(
                "err: Error: cannot hold {DISK}: {DISK} is in use — mounted or held by another program\n"
            )]
        );
    }

    /// A hold nobody can see is useless: if the line cannot be written (the
    /// reader is gone), the disk is let go at once and the exit says so.
    #[test]
    fn a_hold_that_cannot_be_announced_is_released_at_once() {
        for (fail_write, fail_flush) in [(true, false), (false, true)] {
            let log = Log::default();
            let mut out = Recorder::new(&log, "out");
            out.fail_write = fail_write;
            out.fail_flush = fail_flush;
            let code = hold_with(
                Path::new(DISK),
                9,
                opener(&log),
                waiter(&log, Ok(libc::SIGTERM)),
                &mut out,
                &mut Recorder::new(&log, "err"),
            );
            assert_eq!(code, 2);
            let entries = log.entries();
            assert!(!entries.contains(&"wait".to_string()), "{entries:?}");
            let closed = entries.iter().position(|e| e == "closed").unwrap();
            let said = entries
                .iter()
                .position(|e| {
                    e.starts_with(&format!(
                        "err: Error: could not announce the hold on {DISK}: Broken pipe"
                    ))
                })
                .unwrap_or_else(|| panic!("no error line: {entries:?}"));
            assert!(closed < said, "released before saying so: {entries:?}");
        }
    }

    #[test]
    fn a_failed_wait_releases_the_disk_and_fails() {
        let log = Log::default();
        let code = hold_with(
            Path::new(DISK),
            3,
            opener(&log),
            waiter(&log, Err(io::Error::from_raw_os_error(libc::EINVAL))),
            &mut Recorder::new(&log, "out"),
            &mut Recorder::new(&log, "err"),
        );
        assert_eq!(code, 2);
        let entries = log.entries();
        assert_eq!(entries[entries.len() - 2], "closed");
        assert_eq!(
            entries[entries.len() - 1],
            format!(
                "err: Error: waiting for a termination signal failed: Invalid argument (os error 22) — released {DISK}\n"
            )
        );
    }

    // ----- the signals ---------------------------------------------------------

    /// Each signal raised at a thread that blocked them is queued and returned
    /// by `wait`. In a thread of its own, so the mask goes with it.
    #[test]
    fn each_termination_signal_is_queued_until_waited_for() {
        std::thread::spawn(|| {
            let signals = TerminationSignals::block().unwrap();
            for signal in TERMINATION_SIGNALS {
                // SAFETY: directed at this thread only, which blocks `signal`.
                assert_eq!(
                    unsafe { libc::pthread_kill(libc::pthread_self(), signal) },
                    0
                );
                assert_eq!(signals.wait().unwrap(), signal);
            }
        })
        .join()
        .unwrap();
    }

    /// A parent that ignores these signals (bash does SIGINT for every
    /// background job) must not be able to make a hold deaf to them: blocking
    /// also restores their default disposition, so they queue for `wait`.
    #[test]
    fn an_inherited_ignore_does_not_survive_blocking() {
        std::thread::spawn(|| {
            let disposition = |signal| {
                // SAFETY: a null new action only reads the current one.
                let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
                assert_eq!(
                    unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) },
                    0
                );
                old.sa_sigaction
            };
            // SAFETY: SIGHUP's disposition is process-wide; nothing in the test
            // binary relies on it, and the default is what it started with.
            unsafe { libc::signal(libc::SIGHUP, libc::SIG_IGN) };
            assert_eq!(disposition(libc::SIGHUP), libc::SIG_IGN);
            let signals = TerminationSignals::block().unwrap();
            assert_eq!(disposition(libc::SIGHUP), libc::SIG_DFL);
            // SAFETY: directed at this thread only, which blocks SIGHUP.
            assert_eq!(
                unsafe { libc::pthread_kill(libc::pthread_self(), libc::SIGHUP) },
                0
            );
            assert_eq!(signals.wait().unwrap(), libc::SIGHUP);
        })
        .join()
        .unwrap();
    }

    /// The whole command on something that is not a disk: refused with 2.
    #[test]
    fn run_refuses_what_is_not_a_block_device() {
        std::thread::spawn(|| {
            let dir = tempfile::tempdir().unwrap();
            let img = dir.path().join("disk.img");
            std::fs::write(&img, b"x").unwrap();
            assert_eq!(run(&img), 2);
            assert_eq!(run(&dir.path().join("absent")), 2);
        })
        .join()
        .unwrap();
    }
}
