//! The DAS maintenance interlock — `/run/das-maintenance.lock` — and
//! [`MaintenanceHeld`], the proof that this process holds it.
//!
//! Every job that mounts a backup target takes this lock first, so no two of
//! them mount, use and unmount the same targets at once
//! (`.claude/rules/backup.md` §Maintenance Interlock). Mounting a target
//! requires a [`MaintenanceHeld`] — `mount::ensure_targets_mounted` takes one —
//! and the only ways to get one are to take the lock, or, for a command run by
//! a process that holds it, to be handed that process's descriptor on it. A
//! code path that skips the lock does not compile.
//!
//! It matters beyond tidiness (bd DAS-Backup-Manager-frb): a recovery drive
//! lent to a VM is mounted read-write by the guest's kernel while the VM
//! session holds this lock, and a host mount of that filesystem by a second
//! kernel corrupts it.
//!
//! Whoever takes the lock writes one line into the lock file saying who it is
//! (`btrdasd restore browse pid 4242`) and empties it again before letting go,
//! so a job that finds the lock held can say what it is waiting for.

use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::progress::{LogLevel, OrderedProgress, ProgressCallback};
use crate::scrub::{FileLock, MAINTENANCE_LOCK_PATH, ScrubError};

/// Exit code of `btrdasd walk` and the `restore` commands run with
/// `--no-wait` while the lock is held: nothing was mounted or changed.
/// `EX_TEMPFAIL` from sysexits.h — try again later.
pub const DEFERRED_EXIT_CODE: i32 = 75;

/// A process that holds the lock hands it to a command it runs by setting
/// this to the number of its open descriptor on the lock file —
/// `backup-run.sh` does, for `btrdasd walk`. Checked, never believed: see
/// [`MaintenanceHeld::delegated_at`].
pub const DELEGATED_FD_ENV: &str = "DAS_MAINTENANCE_LOCK_FD";

/// How often a waiting job looks at the lock again, and at whether it has
/// been cancelled.
pub const WAIT_POLL: Duration = Duration::from_secs(1);

/// The holder, when the lock file names nobody.
pub const UNKNOWN_HOLDER: &str = "an unknown holder";

/// A record longer than this is cut: it is shown to people, not parsed.
const MAX_RECORD_CHARS: usize = 200;

/// How to free the lock when a job cannot wait for a scrub —
/// `.claude/rules/backup.md` §Sentinel Interaction: `cachyos-sentinel`
/// restarts a stopped unit, so it is masked too, and unmasked once done.
const STOP_A_SCRUB: &str = "If this cannot wait and a scrub holds it: systemctl stop \
     das-scrub.service && systemctl mask das-scrub.service (stop alone is undone within \
     seconds), then systemctl unmask das-scrub.service once done — the next scrub run \
     resumes where this one stopped";

/// Where the lock is and how often a waiting job looks at it again: the
/// production lock in production, a scratch file in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockSite {
    pub path: PathBuf,
    pub poll: Duration,
}

impl LockSite {
    /// `/run/das-maintenance.lock`, looked at every [`WAIT_POLL`].
    pub fn production() -> Self {
        Self {
            path: PathBuf::from(MAINTENANCE_LOCK_PATH),
            poll: WAIT_POLL,
        }
    }
}

/// Where a command's callers are found: a `/proc`, and the process to start
/// from — the command's parent. [`wait_for`] refuses a lock that one of them
/// holds without handing it down: it would never be let go while the command
/// waits for it.
#[derive(Debug, Clone)]
pub struct Callers {
    pub proc_root: PathBuf,
    pub parent: u32,
}

impl Callers {
    /// This process's callers, from `/proc`.
    pub fn of_this_process() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            parent: std::os::unix::process::parent_id(),
        }
    }

    /// `(pid, record)` when the record in the lock file at `path` names one of
    /// these callers. The record only names: whether the lock is held is the
    /// asker's to know.
    fn recorded_in(&self, path: &Path) -> Option<(u32, String)> {
        // Display only, as for every reader of the record: unreadable is empty.
        let record = record_line(&std::fs::read_to_string(path).unwrap_or_default());
        let pid = record_pid(&record)?;
        ancestors(&self.proc_root, self.parent)
            .contains(&pid)
            .then_some((pid, record))
    }
}

/// Proof that this process holds the maintenance lock, for as long as the
/// value lives. Everything that mounts a backup target requires one.
#[derive(Debug)]
pub struct MaintenanceHeld {
    path: PathBuf,
    how: How,
}

#[derive(Debug)]
enum How {
    /// This process took the lock. Its record is in the lock file; both go
    /// on drop.
    Owned(FileLock),
    /// The process that started this one holds the lock and handed it down.
    /// Never unlocked here: the open file description is shared with that
    /// process, and unlocking it would release the lock it still relies on.
    Delegated,
    /// Test code that mounts nothing real.
    #[cfg(test)]
    Assumed,
}

impl MaintenanceHeld {
    /// `job` names what took it — `btrdasd restore browse` — and is
    /// recorded with this process's pid.
    fn owned(lock: FileLock, job: &str) -> Self {
        // Display only: a record that cannot be written costs the name,
        // never the lock — a waiter then reads the holder as unknown. Said,
        // so the missing name has a reason.
        if let Err(e) = lock.write_note(&format!("{job} pid {}\n", std::process::id())) {
            eprintln!(
                "warning: could not record {job} as the holder of {}: {e}",
                lock.path().display()
            );
        }
        Self {
            path: lock.path().to_path_buf(),
            how: How::Owned(lock),
        }
    }

    /// Take the lock if it is free; `None` when another holds it. For jobs
    /// that defer rather than wait (reconcile, doctor).
    pub fn try_acquire_at(path: &Path, job: &str) -> Result<Option<Self>, ScrubError> {
        Ok(FileLock::try_acquire(path)?.map(|lock| Self::owned(lock, job)))
    }

    /// Take the lock, waiting as long as it takes — backups and scrubs. The
    /// line it logs if it has to wait names the holder ([`blocked_line`]).
    pub fn acquire_blocking_at(
        path: &Path,
        job: &str,
        progress: &dyn ProgressCallback,
    ) -> Result<Self, ScrubError> {
        FileLock::acquire_blocking(path, progress, &|| blocked_line(path))
            .map(|lock| Self::owned(lock, job))
    }

    /// The lock as the process that started this one holds it: `fd` is that
    /// process's open descriptor on the lock file, inherited and named in
    /// [`DELEGATED_FD_ENV`]. Proven, never believed, in three steps: `fd` must
    /// be the lock file itself (device and inode); the lock must be held — a
    /// fresh open of the file cannot take it; and it must be held through
    /// `fd` — taking it through a duplicate of `fd` succeeds at once, which it
    /// does only for the open file description that holds it. A free lock is
    /// refused rather than taken: a descriptor that held nothing must not end
    /// up holding the lock for its owner's lifetime. Anything else is an
    /// error, never a fall-back to waiting: a command that waited for a lock
    /// its own parent holds would wait forever.
    ///
    /// Not atomic: a holder that lets go between the second and third steps
    /// would let a descriptor that held nothing take the lock. The caller that
    /// named it then holds it until it exits, with no record — readers show
    /// [`UNKNOWN_HOLDER`], and a backup waits on it. Inherent to `flock`, which
    /// has no "lock only if held by this description".
    pub fn delegated_at(path: &Path, fd: RawFd) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        let lock = path.display();
        let want = std::fs::metadata(path).map_err(|e| format!("cannot stat {lock}: {e}"))?;
        // SAFETY: `fstat` writes one `stat` through a pointer to a zeroed
        // one; a descriptor that is not open is EBADF, not undefined
        // behaviour.
        let mut got: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut got) } != 0 {
            return Err(format!(
                "{DELEGATED_FD_ENV}={fd} is not an open descriptor ({})",
                std::io::Error::last_os_error()
            ));
        }
        if got.st_dev != want.dev() || got.st_ino != want.ino() {
            return Err(format!("{DELEGATED_FD_ENV}={fd} is not {lock}"));
        }
        // A fresh open that can take the lock finds it free: then `fd` holds
        // nothing. The probe lets go when it drops.
        if FileLock::try_acquire(path)
            .map_err(|e| format!("{DELEGATED_FD_ENV}={fd}: cannot look at {lock}: {e}"))?
            .is_some()
        {
            return Err(format!(
                "{DELEGATED_FD_ENV}={fd} does not hold {lock}: the lock is free"
            ));
        }
        // Held, then — by whom? A duplicate of `fd` shares its open file
        // description, so locking through it succeeds only if that description
        // is the holder. Closing the duplicate releases nothing: the caller's
        // descriptor still refers to the description.
        // SAFETY: `fstat` above proved `fd` open, and nothing in this process
        // closes it during this one borrow.
        let shared = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .map_err(|e| format!("{DELEGATED_FD_ENV}={fd} cannot be duplicated ({e})"))?;
        if let Err(e) = std::fs::File::from(shared).try_lock() {
            return Err(format!(
                "{DELEGATED_FD_ENV}={fd} does not hold {lock} ({e})"
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            how: How::Delegated,
        })
    }

    /// A proof for tests that mount nothing real.
    #[cfg(test)]
    pub(crate) fn assumed() -> Self {
        Self {
            path: PathBuf::from(MAINTENANCE_LOCK_PATH),
            how: How::Assumed,
        }
    }

    /// A hold handed down by the process that started this one.
    #[cfg(test)]
    pub(crate) fn delegated_for_test() -> Self {
        Self {
            path: PathBuf::from(MAINTENANCE_LOCK_PATH),
            how: How::Delegated,
        }
    }

    /// Whether the process that started this one holds the lock and handed
    /// it down (`DAS_MAINTENANCE_LOCK_FD`), as `backup-run.sh` does.
    pub fn is_delegated(&self) -> bool {
        matches!(self.how, How::Delegated)
    }

    /// The lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for MaintenanceHeld {
    fn drop(&mut self) {
        // Empty the record while the lock is still held: once it is released
        // the next holder may already have written its own. Display only. A
        // delegated hold's record belongs to the process that holds it.
        if let How::Owned(lock) = &self.how
            && let Err(e) = lock.clear_note()
        {
            eprintln!(
                "warning: could not empty the holder record in {}: {e}",
                lock.path().display()
            );
        }
        // The `FileLock` is dropped after this, which releases the lock.
    }
}

/// A set of locks that includes the maintenance lock.
pub trait HoldsMaintenance {
    fn maintenance(&self) -> &MaintenanceHeld;
}

impl HoldsMaintenance for MaintenanceHeld {
    fn maintenance(&self) -> &MaintenanceHeld {
        self
    }
}

/// How a wait for the lock ended.
#[derive(Debug)]
pub enum Waited {
    /// This process holds the lock now.
    Held(MaintenanceHeld),
    /// Asked not to wait, and `holder` holds it. Nothing was done.
    Deferred { holder: String },
    /// Cancelled while `holder` held it. Nothing was done.
    Cancelled { holder: String },
    /// Held by one of the [`Callers`] — `pid`, recorded as `record` — that
    /// did not hand its hold down: waiting would never end. Nothing was done.
    CallerHolds { pid: u32, record: String },
}

/// Take the lock for an interactive job — `walk` and `restore`, from the CLI
/// or the GUI: at once if it is free. Otherwise, if it is held by one of
/// `callers`, stop ([`Waited::CallerHolds`]); with `no_wait`, return at once;
/// without, log one line naming the holder and how to stop a scrub, then look
/// again every `site.poll` until the lock is free or `cancelled()` says to
/// stop. `job` is recorded as the holder once the lock is taken. The lock is
/// only ever taken to be kept: nothing here takes it to look and lets go.
pub fn wait_for(
    site: &LockSite,
    job: &str,
    no_wait: bool,
    callers: Option<&Callers>,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn ProgressCallback,
) -> Result<Waited, ScrubError> {
    if let Some(held) = MaintenanceHeld::try_acquire_at(&site.path, job)? {
        return Ok(Waited::Held(held));
    }
    // Refused, so it is held — no probe needed to know. Held by a caller of
    // this command, as its record says, it would never be let go while the
    // command waited. Unless that caller let go since the refusal: so one
    // more take, kept if it succeeds.
    if let Some((pid, record)) = callers.and_then(|callers| callers.recorded_in(&site.path)) {
        if let Some(held) = MaintenanceHeld::try_acquire_at(&site.path, job)? {
            return Ok(Waited::Held(held));
        }
        return Ok(Waited::CallerHolds { pid, record });
    }
    let holder = holder_of(&site.path);
    if no_wait {
        return Ok(Waited::Deferred { holder });
    }
    progress.on_log(LogLevel::Warning, &waiting_line(&site.path, &holder));
    let started = Instant::now();
    loop {
        std::thread::sleep(site.poll);
        if cancelled() {
            return Ok(Waited::Cancelled { holder });
        }
        if let Some(held) = MaintenanceHeld::try_acquire_at(&site.path, job)? {
            progress.on_log(
                LogLevel::Info,
                &format!(
                    "DAS maintenance lock acquired after waiting {}s",
                    started.elapsed().as_secs()
                ),
            );
            return Ok(Waited::Held(held));
        }
    }
}

/// The lock for a GUI job (the D-Bus helper): [`wait_for`] it, and stop
/// waiting as soon as the job is cancelled. Nothing is mounted yet, so this
/// is the one point at which a cancel can act at once. `Err` is the job's
/// summary.
pub fn hold_for_job(
    site: &LockSite,
    job: &str,
    progress: &OrderedProgress,
) -> Result<MaintenanceHeld, String> {
    let cancelled = || progress.is_cancelled();
    // No callers to check: the helper's are the service manager's.
    match wait_for(site, job, false, None, &cancelled, progress) {
        Ok(Waited::Held(held)) => Ok(held),
        // `Deferred` needs `no_wait`, `CallerHolds` callers; either way
        // nothing was done.
        Ok(Waited::Cancelled { holder } | Waited::Deferred { holder }) => Err(format!(
            "stopped waiting for the DAS maintenance lock, held by {holder}; nothing was mounted"
        )),
        Ok(Waited::CallerHolds { pid, record }) => Err(caller_holds_line(pid, &record)),
        Err(e) => Err(format!("Could not take the DAS maintenance lock: {e}")),
    }
}

/// Who holds the lock, as the holder recorded itself in the lock file — or
/// [`UNKNOWN_HOLDER`] when nothing is recorded; when the record carries no
/// pid, so nothing says whether it still runs (shown as the last one
/// recorded); or when the recorded process has exited (a holder that did not
/// record itself, such as a plain `flock`, holds it after one that did).
pub fn holder_of(path: &Path) -> String {
    // Display only: a record that cannot be read is an unknown holder, and
    // the caller waits, or defers, just the same.
    let note = std::fs::read_to_string(path).unwrap_or_default();
    holder_from_note(&note, &pid_is_running)
}

/// [`holder_of`] for a record already read: its first line, without control
/// characters and cut to [`MAX_RECORD_CHARS`]. `running` says whether a pid
/// is a live process.
fn holder_from_note(note: &str, running: &dyn Fn(u32) -> bool) -> String {
    let line = record_line(note);
    if line.is_empty() {
        return UNKNOWN_HOLDER.to_string();
    }
    // Every holder this project writes records its pid; a record without one
    // cannot be checked, so it is only ever the last one recorded.
    match record_pid(&line) {
        Some(pid) if running(pid) => line,
        Some(_) => {
            format!("{UNKNOWN_HOLDER} (the last recorded holder, {line}, is no longer running)")
        }
        None => format!("{UNKNOWN_HOLDER} (last recorded: {line})"),
    }
}

/// A record's first line, without control characters, cut to
/// [`MAX_RECORD_CHARS`] and trimmed.
fn record_line(note: &str) -> String {
    let line: String = note
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_RECORD_CHARS)
        .collect();
    line.trim().to_string()
}

/// The pid a record ends with — `… pid 4242`.
fn record_pid(line: &str) -> Option<u32> {
    line.rsplit_once(" pid ")?.1.parse().ok()
}

fn pid_is_running(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// `start` and its ancestors, up to but not including init, read from
/// `<proc_root>/<pid>/stat`. A process that cannot be read, or a loop, ends it.
fn ancestors(proc_root: &Path, start: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut pid = start;
    while pid > 1 && !chain.contains(&pid) {
        chain.push(pid);
        let Some(parent) = parent_of(proc_root, pid) else {
            break;
        };
        pid = parent;
    }
    chain
}

/// The parent in `<proc_root>/<pid>/stat` — the field after the state, read
/// after the last `)`, because the command name may hold spaces and both
/// parentheses.
fn parent_of(proc_root: &Path, pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Why a command refuses to wait for a lock its own caller holds.
pub fn caller_holds_line(pid: u32, record: &str) -> String {
    format!(
        "the DAS maintenance lock is held by my own caller (pid {pid}, {record}); it must hand \
         the hold down via {DELEGATED_FD_ENV}"
    )
}

/// What a backup or a scrub logs while it waits for the lock.
pub fn blocked_line(path: &Path) -> String {
    format!(
        "Waiting for the DAS maintenance lock {}, held by {}",
        path.display(),
        holder_of(path)
    )
}

/// The line a job logs before it starts waiting.
pub fn waiting_line(path: &Path, holder: &str) -> String {
    format!(
        "Waiting for the DAS maintenance lock {}, held by {holder}; the backup targets are \
         mounted once it is free. {STOP_A_SCRUB}",
        path.display()
    )
}

/// What `--no-wait` prints when the lock is held.
pub fn deferred_line(path: &Path, holder: &str) -> String {
    format!(
        "Deferred — the DAS maintenance lock {} is held by {holder}; nothing was mounted \
         (--no-wait, exit {DEFERRED_EXIT_CODE})",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs::OpenOptions;
    use std::os::fd::{AsFd, AsRawFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Mutex};

    use crate::progress::{LogLevel, NullProgress, ProgressEvent, ProgressSink};

    /// How often the scratch lock is looked at — short, so a test that waits
    /// takes milliseconds.
    const POLL: Duration = Duration::from_millis(10);
    /// Long enough to be sure a waiter is blocked, not merely slow to start.
    const STILL_WAITING: Duration = Duration::from_millis(300);
    /// Ample for a waiter to notice a release or a cancel (it looks every
    /// 10 ms).
    const NOTICES: Duration = Duration::from_secs(1);

    /// `f` on a thread: what it returned within `limit`, or `None` while it
    /// still runs. A call that should not wait, but does, then fails its
    /// test instead of hanging it.
    fn within<T: Send + 'static>(
        limit: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(limit).ok()
    }

    fn scratch() -> (tempfile::TempDir, LockSite) {
        let dir = tempfile::tempdir().unwrap();
        let site = LockSite {
            path: dir.path().join("das-maintenance.lock"),
            poll: POLL,
        };
        (dir, site)
    }

    fn note(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn me(job: &str) -> String {
        format!("{job} pid {}", std::process::id())
    }

    /// Records every log line with its level.
    #[derive(Default)]
    struct Logs(Mutex<Vec<(LogLevel, String)>>);

    impl ProgressCallback for Logs {
        fn on_stage(&self, _: &str, _: u64) {}
        fn on_progress(&self, _: u64, _: u64, _: &str) {}
        fn on_throughput(&self, _: u64) {}
        fn on_log(&self, level: LogLevel, message: &str) {
            self.0.lock().unwrap().push((level, message.to_string()));
        }
        fn on_complete(&self, _: bool, _: &str) {}
    }

    impl Logs {
        fn lines(&self) -> Vec<(LogLevel, String)> {
            self.0.lock().unwrap().clone()
        }
    }

    /// A descriptor that holds the lock, as `backup-run.sh`'s fd 8 does, and
    /// lets go of it for good, as a holder in another process does: unlocked,
    /// then closed. Closing alone would not do it here. A fork of this
    /// process copies every descriptor until its exec, and any test's spawn
    /// forks, so such a copy could hold the lock on after the close (bd
    /// DAS-Backup-Manager-eu0) — which no fork of the waiter can do to a
    /// holder in another process. `FileLock` lets go the same way.
    struct Holding(std::fs::File);

    impl Drop for Holding {
        fn drop(&mut self) {
            // SAFETY: flock on a descriptor `self.0` owns until just after.
            unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }

    impl AsRawFd for Holding {
        fn as_raw_fd(&self) -> RawFd {
            self.0.as_raw_fd()
        }
    }

    impl AsFd for Holding {
        fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    fn holding_descriptor(path: &Path) -> Holding {
        Holding(locked_file(path))
    }

    /// A descriptor that holds the lock and lets go only when it closes.
    fn locked_file(path: &Path) -> std::fs::File {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        // SAFETY: flock on a descriptor `file` owns.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "the fixture must hold the lock");
        file
    }

    /// The fixture lets go for good even while a copy of its descriptor lives
    /// on; the converse — a descriptor merely closed — is still held by the
    /// copy, which is what made the test above flaky.
    #[test]
    fn a_holder_lets_go_even_while_a_fork_holds_a_copy_of_its_descriptor() {
        let (_dir, site) = scratch();
        let holder = holding_descriptor(&site.path);
        let copy = holder.as_fd().try_clone_to_owned().unwrap();
        drop(holder);
        assert!(
            MaintenanceHeld::try_acquire_at(&site.path, "x")
                .unwrap()
                .is_some(),
            "let go for good"
        );
        drop(copy);
        let closed_only = locked_file(&site.path);
        let copy = closed_only.as_fd().try_clone_to_owned().unwrap();
        drop(closed_only);
        assert!(
            MaintenanceHeld::try_acquire_at(&site.path, "x")
                .unwrap()
                .is_none(),
            "merely closed: the copy holds it"
        );
        drop(copy);
    }

    // --- the site ----------------------------------------------------------

    #[test]
    fn the_production_site_is_the_shared_lock_looked_at_every_second() {
        let site = LockSite::production();
        assert_eq!(site.path, Path::new("/run/das-maintenance.lock"));
        assert_eq!(site.poll, Duration::from_secs(1));
    }

    // --- taking the lock records the holder --------------------------------

    #[test]
    fn whoever_takes_the_lock_records_itself_and_clears_the_record_on_release() {
        let (_dir, site) = scratch();
        let held = MaintenanceHeld::try_acquire_at(&site.path, "btrdasd restore browse")
            .unwrap()
            .expect("the lock is free");
        assert_eq!(held.path(), site.path);
        assert_eq!(
            note(&site.path),
            format!("{}\n", me("btrdasd restore browse"))
        );

        // It is really held, and a refused taker leaves the record alone.
        assert!(
            MaintenanceHeld::try_acquire_at(&site.path, "btrdasd walk")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            note(&site.path),
            format!("{}\n", me("btrdasd restore browse"))
        );

        drop(held);
        assert_eq!(note(&site.path), "", "the record goes with the lock");
        assert!(
            MaintenanceHeld::try_acquire_at(&site.path, "btrdasd walk")
                .unwrap()
                .is_some(),
            "and the lock is free again"
        );
    }

    #[test]
    fn a_blocking_taker_records_itself_too() {
        let (_dir, site) = scratch();
        let held =
            MaintenanceHeld::acquire_blocking_at(&site.path, "btrdasd scrub run", &NullProgress)
                .unwrap();
        assert_eq!(note(&site.path), format!("{}\n", me("btrdasd scrub run")));
        drop(held);
        assert_eq!(note(&site.path), "");
    }

    // --- waiting -----------------------------------------------------------

    #[test]
    fn a_free_lock_is_taken_at_once_without_a_waiting_line() {
        let (_dir, site) = scratch();
        let logs = Arc::new(Logs::default());
        let (waiter_site, waiter_logs) = (site.clone(), logs.clone());
        let waited = within(STILL_WAITING, move || {
            wait_for(
                &waiter_site,
                "btrdasd walk",
                false,
                None,
                &|| false,
                &*waiter_logs,
            )
        })
        .expect("a free lock is taken at once")
        .unwrap();
        assert!(matches!(waited, Waited::Held(_)), "{waited:?}");
        assert!(logs.lines().is_empty(), "{:?}", logs.lines());
        assert_eq!(note(&site.path), format!("{}\n", me("btrdasd walk")));
    }

    #[test]
    fn no_wait_defers_at_once_naming_the_holder_and_takes_nothing() {
        let (_dir, site) = scratch();
        let scrub = MaintenanceHeld::try_acquire_at(&site.path, "btrdasd scrub run")
            .unwrap()
            .unwrap();
        // On a thread, so a wait where none belongs fails the test rather
        // than hanging it.
        let logs = Arc::new(Logs::default());
        let (tx, rx) = mpsc::channel();
        let (waiter_site, waiter_logs) = (site.clone(), logs.clone());
        std::thread::spawn(move || {
            let _ = tx.send(wait_for(
                &waiter_site,
                "btrdasd walk",
                true,
                None,
                &|| false,
                &*waiter_logs,
            ));
        });
        match rx
            .recv_timeout(STILL_WAITING)
            .expect("defers at once")
            .unwrap()
        {
            Waited::Deferred { holder } => assert_eq!(holder, me("btrdasd scrub run")),
            other => panic!("expected Deferred, got {other:?}"),
        }
        assert!(logs.lines().is_empty(), "deferring announces no wait");
        assert_eq!(note(&site.path), format!("{}\n", me("btrdasd scrub run")));
        drop(scrub);
    }

    #[test]
    fn a_held_lock_is_waited_for_and_taken_once_released() {
        let (_dir, site) = scratch();
        let scrub = MaintenanceHeld::try_acquire_at(&site.path, "btrdasd scrub run")
            .unwrap()
            .unwrap();
        let logs = Arc::new(Logs::default());
        let (tx, rx) = mpsc::channel();
        let (waiter_site, waiter_logs) = (site.clone(), logs.clone());
        let waiter = std::thread::spawn(move || {
            let waited = wait_for(
                &waiter_site,
                "btrdasd restore file",
                false,
                None,
                &|| false,
                &*waiter_logs,
            );
            tx.send(waited).unwrap();
        });

        assert!(
            matches!(
                rx.recv_timeout(STILL_WAITING),
                Err(RecvTimeoutError::Timeout)
            ),
            "must still be waiting while the lock is held"
        );
        let lines = logs.lines();
        assert_eq!(lines.len(), 1, "one line before waiting: {lines:?}");
        assert_eq!(lines[0].0, LogLevel::Warning);
        assert!(
            lines[0]
                .1
                .contains(&format!("held by {}", me("btrdasd scrub run"))),
            "{}",
            lines[0].1
        );

        drop(scrub);
        let waited = rx
            .recv_timeout(NOTICES)
            .expect("takes the lock once it is free")
            .unwrap();
        assert!(matches!(waited, Waited::Held(_)), "{waited:?}");
        assert_eq!(
            note(&site.path),
            format!("{}\n", me("btrdasd restore file"))
        );
        let lines = logs.lines();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[1].1.contains("acquired after waiting"),
            "{}",
            lines[1].1
        );
        waiter.join().unwrap();
    }

    #[test]
    fn a_cancelled_wait_stops_at_once_and_takes_nothing() {
        let (_dir, site) = scratch();
        let backup = MaintenanceHeld::try_acquire_at(&site.path, "backup-run.sh")
            .unwrap()
            .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (waiter_site, waiter_cancel) = (site.clone(), cancel.clone());
        let waiter = std::thread::spawn(move || {
            let cancelled = || waiter_cancel.load(Ordering::SeqCst);
            let waited = wait_for(&waiter_site, "job", false, None, &cancelled, &NullProgress);
            tx.send(waited).unwrap();
        });

        assert!(matches!(
            rx.recv_timeout(STILL_WAITING),
            Err(RecvTimeoutError::Timeout)
        ));
        cancel.store(true, Ordering::SeqCst);
        match rx
            .recv_timeout(NOTICES)
            .expect("stops once cancelled")
            .unwrap()
        {
            Waited::Cancelled { holder } => assert_eq!(holder, me("backup-run.sh")),
            other => panic!("expected Cancelled, got {other:?}"),
        }
        // The holder still holds it, and its record is intact.
        assert!(
            MaintenanceHeld::try_acquire_at(&site.path, "x")
                .unwrap()
                .is_none()
        );
        assert_eq!(note(&site.path), format!("{}\n", me("backup-run.sh")));
        drop(backup);
        waiter.join().unwrap();
    }

    // --- a GUI job ---------------------------------------------------------

    /// Keeps what the job's client would have received.
    struct Client(Arc<Mutex<Vec<ProgressEvent>>>);

    impl ProgressSink for Client {
        fn journal(&mut self, _: LogLevel, _: &str) {}
        fn emit(&mut self, event: ProgressEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn gui_progress() -> (Arc<OrderedProgress>, Arc<Mutex<Vec<ProgressEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(OrderedProgress::new(Client(events.clone()))),
            events,
        )
    }

    #[test]
    fn a_gui_job_takes_a_free_lock() {
        let (_dir, site) = scratch();
        let (progress, _) = gui_progress();
        let job_site = site.clone();
        let held = within(STILL_WAITING, move || {
            hold_for_job(&job_site, "btrdasd-helper IndexWalk job", &progress)
        })
        .expect("a free lock is taken at once")
        .unwrap();
        assert_eq!(
            note(&site.path),
            format!("{}\n", me("btrdasd-helper IndexWalk job"))
        );
        drop(held);
    }

    #[test]
    fn a_cancelled_gui_job_stops_waiting_and_ends_cancelled() {
        let (_dir, site) = scratch();
        let scrub = MaintenanceHeld::try_acquire_at(&site.path, "btrdasd scrub run")
            .unwrap()
            .unwrap();
        let (progress, events) = gui_progress();
        let (tx, rx) = mpsc::channel();
        let (job_site, job_progress) = (site.clone(), progress.clone());
        let job = std::thread::spawn(move || {
            let held = hold_for_job(&job_site, "btrdasd-helper RestoreFiles job", &job_progress);
            tx.send(held.map(|_| ())).unwrap();
        });

        assert!(matches!(
            rx.recv_timeout(STILL_WAITING),
            Err(RecvTimeoutError::Timeout)
        ));
        progress.cancel();
        let summary = rx
            .recv_timeout(NOTICES)
            .expect("stops waiting once cancelled")
            .unwrap_err();
        assert_eq!(
            summary,
            format!(
                "stopped waiting for the DAS maintenance lock, held by {}; nothing was mounted",
                me("btrdasd scrub run")
            )
        );
        job.join().unwrap();
        progress.finish(false, &summary);

        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ProgressEvent::Log { message, .. }
                if message.starts_with("Waiting for the DAS maintenance lock"))),
            "the client saw the waiting line: {events:?}"
        );
        match events.last() {
            Some(ProgressEvent::Finished { success, summary }) => {
                assert!(!success);
                assert!(summary.starts_with("cancelled — "), "{summary}");
                assert!(summary.contains(&me("btrdasd scrub run")), "{summary}");
            }
            other => panic!("the job must end with Finished, got {other:?}"),
        }
        drop(scrub);
    }

    #[test]
    fn a_gui_job_names_a_lock_it_cannot_open() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "").unwrap();
        let site = LockSite {
            path: dir.path().join("file").join("lock"),
            poll: POLL,
        };
        let (progress, _) = gui_progress();
        let err = within(STILL_WAITING, move || hold_for_job(&site, "job", &progress))
            .expect("a lock that cannot be opened fails at once")
            .unwrap_err();
        assert!(
            err.starts_with("Could not take the DAS maintenance lock: "),
            "{err}"
        );
    }

    // --- reading the record ------------------------------------------------

    #[test]
    fn an_empty_or_missing_record_is_an_unknown_holder() {
        assert_eq!(holder_from_note("", &|_| true), UNKNOWN_HOLDER);
        assert_eq!(holder_from_note("  \n  \n", &|_| true), UNKNOWN_HOLDER);
        assert_eq!(
            holder_of(Path::new("/nonexistent-frb/das-maintenance.lock")),
            UNKNOWN_HOLDER
        );
    }

    #[test]
    fn a_record_whose_process_runs_names_it() {
        assert_eq!(
            holder_from_note("btrdasd scrub run pid 42\n", &|pid| pid == 42),
            "btrdasd scrub run pid 42"
        );
    }

    #[test]
    fn a_record_whose_process_has_exited_is_not_presented_as_the_holder() {
        assert_eq!(
            holder_from_note("backup-run.sh pid 42\n", &|_| false),
            "an unknown holder (the last recorded holder, backup-run.sh pid 42, is no longer running)"
        );
    }

    #[test]
    fn a_record_without_a_pid_is_only_the_last_recorded_holder() {
        // Nothing says whether it still runs, so it is never presented as
        // the holder — even where every pid would read as running.
        assert_eq!(
            holder_from_note("recovery-os VM session A\n", &|_| true),
            "an unknown holder (last recorded: recovery-os VM session A)"
        );
        assert_eq!(
            holder_from_note("odd pid 4x\n", &|_| true),
            "an unknown holder (last recorded: odd pid 4x)",
            "a pid that does not parse is no pid"
        );
    }

    #[test]
    fn only_the_first_line_is_read_without_control_characters_and_at_most_200_chars() {
        assert_eq!(
            holder_from_note("  evil\u{1b}[2J name pid 7\t\nsecond\n", &|_| true),
            "evil[2J name pid 7"
        );
        let long = "x".repeat(500);
        assert_eq!(
            holder_from_note(&long, &|_| true),
            format!("{UNKNOWN_HOLDER} (last recorded: {})", "x".repeat(200))
        );
    }

    #[test]
    fn whether_a_process_runs_is_read_from_proc() {
        assert!(pid_is_running(std::process::id()));
        assert!(!pid_is_running(u32::MAX), "beyond pid_max, never a process");
    }

    #[test]
    fn the_holder_is_read_from_the_lock_file() {
        let (_dir, site) = scratch();
        let held = MaintenanceHeld::try_acquire_at(&site.path, "btrdasd walk")
            .unwrap()
            .unwrap();
        assert_eq!(holder_of(&site.path), me("btrdasd walk"));
        drop(held);
        assert_eq!(holder_of(&site.path), UNKNOWN_HOLDER);
    }

    // --- what is said ------------------------------------------------------

    #[test]
    fn the_waiting_line_names_the_lock_the_holder_and_how_to_stop_a_scrub() {
        let line = waiting_line(
            Path::new("/run/das-maintenance.lock"),
            "btrdasd scrub run pid 9",
        );
        for part in [
            "Waiting for the DAS maintenance lock /run/das-maintenance.lock",
            "held by btrdasd scrub run pid 9",
            "systemctl stop das-scrub.service && systemctl mask das-scrub.service",
            "stop alone is undone within seconds",
            "systemctl unmask das-scrub.service",
        ] {
            assert!(line.contains(part), "{part:?} missing from: {line}");
        }
    }

    #[test]
    fn the_deferred_line_names_the_holder_and_the_exit_code() {
        let line = deferred_line(
            Path::new("/run/das-maintenance.lock"),
            "btrdasd scrub run pid 9",
        );
        for part in [
            "Deferred",
            "/run/das-maintenance.lock",
            "held by btrdasd scrub run pid 9",
            "nothing was mounted",
            "--no-wait",
            "exit 75",
        ] {
            assert!(line.contains(part), "{part:?} missing from: {line}");
        }
    }

    /// A record that cannot be written costs the name, never the lock: the
    /// hold is taken and kept, and the failure is said on stderr (both
    /// `set_len` calls fail on a character device).
    #[test]
    fn a_record_that_cannot_be_written_still_leaves_the_lock_held() {
        let full = Path::new("/dev/full");
        let held = MaintenanceHeld::try_acquire_at(full, "btrdasd walk")
            .unwrap()
            .expect("nobody else locks /dev/full");
        assert!(
            FileLock::try_acquire(full).unwrap().is_none(),
            "the hold is real although its record was not written"
        );
        drop(held);
        assert!(FileLock::try_acquire(full).unwrap().is_some(), "and let go");
    }

    // --- the caller holds it --------------------------------------------------

    /// A fake `/proc`: each `(pid, ppid, comm)` gets a `stat` line.
    fn fake_proc(entries: &[(u32, u32, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (pid, ppid, comm) in entries {
            let process = dir.path().join(pid.to_string());
            std::fs::create_dir(&process).unwrap();
            std::fs::write(
                process.join("stat"),
                format!("{pid} ({comm}) S {ppid} {pid} {pid} 0 -1 4194560 0 0\n"),
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn the_ancestry_runs_from_the_parent_up_to_init() {
        let proc = fake_proc(&[
            (300, 200, "btrdasd"),
            (200, 100, "bash"),
            (100, 1, "odd) S 9 (name"),
            (1, 0, "systemd"),
        ]);
        assert_eq!(ancestors(proc.path(), 300), [300, 200, 100]);
        assert_eq!(
            parent_of(proc.path(), 100),
            Some(1),
            "the parent is read after the last ')' — a name may hold both"
        );
        assert_eq!(ancestors(proc.path(), 999), [999], "a gone process ends it");
        let looped = fake_proc(&[(10, 11, "a"), (11, 10, "b")]);
        assert_eq!(ancestors(looped.path(), 10), [10, 11], "a loop ends it");
    }

    fn callers_from(proc: &tempfile::TempDir, parent: u32) -> Callers {
        Callers {
            proc_root: proc.path().to_path_buf(),
            parent,
        }
    }

    /// Held, and recorded by one of the callers: refused at once, with or
    /// without `no_wait`, before any waiting line. Anyone else is deferred to
    /// (or waited for) as before — and so is everyone when no callers are
    /// given, as for the GUI's jobs.
    #[test]
    fn a_lock_held_by_a_caller_is_refused_at_once() {
        let (_dir, site) = scratch();
        let proc = fake_proc(&[(300, 200, "bash"), (200, 100, "bash"), (100, 1, "bash")]);
        let holder = holding_descriptor(&site.path);
        let waited = |record: &str, no_wait: bool, callers: Option<Callers>| {
            std::fs::write(&site.path, format!("{record}\n")).unwrap();
            let logs = Arc::new(Logs::default());
            let (waiter_site, waiter_logs) = (site.clone(), logs.clone());
            // On a thread, so a wait where none belongs fails the test
            // rather than hanging it.
            let waited = within(STILL_WAITING, move || {
                wait_for(
                    &waiter_site,
                    "btrdasd walk",
                    no_wait,
                    callers.as_ref(),
                    &|| false,
                    &*waiter_logs,
                )
            })
            .unwrap_or_else(|| panic!("{record}: waited where it must not"))
            .unwrap();
            assert!(logs.lines().is_empty(), "{record}: {:?}", logs.lines());
            assert_eq!(note(&site.path), format!("{record}\n"), "left alone");
            waited
        };
        for no_wait in [false, true] {
            match waited(
                "backup-run.sh pid 200",
                no_wait,
                Some(callers_from(&proc, 300)),
            ) {
                Waited::CallerHolds { pid, record } => {
                    assert_eq!((pid, record.as_str()), (200, "backup-run.sh pid 200"));
                }
                other => panic!("expected CallerHolds, got {other:?}"),
            }
            assert!(
                matches!(
                    waited("x pid 300", no_wait, Some(callers_from(&proc, 300))),
                    Waited::CallerHolds { pid: 300, .. }
                ),
                "the parent itself"
            );
        }
        for record in [
            "btrdasd scrub run pid 777",
            "x pid 1",
            "recovery-os VM session A",
        ] {
            assert!(
                matches!(
                    waited(record, true, Some(callers_from(&proc, 300))),
                    Waited::Deferred { .. }
                ),
                "not a caller: {record}"
            );
        }
        assert!(
            matches!(
                waited("backup-run.sh pid 200", true, None),
                Waited::Deferred { .. }
            ),
            "no callers given, none checked"
        );
        drop(holder);
    }

    /// A free lock is taken at the first attempt, which keeps it: nothing
    /// probes it first, even when a stale record names one of the callers.
    #[test]
    fn a_free_lock_is_taken_once_whatever_the_record_names() {
        let (_dir, site) = scratch();
        let proc = fake_proc(&[(300, 200, "btrdasd"), (200, 100, "bash"), (100, 1, "bash")]);
        // Left by a caller that has let go: nobody holds the lock.
        std::fs::write(&site.path, "backup-run.sh pid 200\n").unwrap();
        let callers = callers_from(&proc, 300);
        let never = || false;
        let waited = wait_for(
            &site,
            "btrdasd walk",
            false,
            Some(&callers),
            &never,
            &NullProgress,
        )
        .unwrap();
        assert!(matches!(waited, Waited::Held(_)), "{waited:?}");
        assert_eq!(
            crate::scrub::opens::count(&site.path),
            1,
            "one attempt, which took it: never probed first"
        );
    }

    /// One run of a caller that lets go between the waiter's refused take and
    /// its reading of the caller's record. With `fork_copy`, a duplicate of
    /// the caller's descriptor — what a fork of this process holds until its
    /// exec — outlives the letting go until the waiter has decided. What the
    /// wait ended in, and how often the lock file was opened to take it.
    fn caller_lets_go_meanwhile(fork_copy: bool) -> (Waited, usize) {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        let (_dir, site) = scratch();
        let proc = fake_proc(&[(300, 200, "btrdasd"), (100, 1, "bash")]);
        // 200's stat is a FIFO: reading it blocks until the caller below
        // answers — after the refused take and the record were read.
        std::fs::create_dir(proc.path().join("200")).unwrap();
        let stat = proc.path().join("200").join("stat");
        let fifo_path = std::ffi::CString::new(stat.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path; mkfifo touches nothing else.
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        std::fs::write(&site.path, "backup-run.sh pid 200\n").unwrap();
        let holder = holding_descriptor(&site.path);
        let caller = std::thread::spawn(move || {
            // Once the waiter reads 200's stat, let go — then answer it.
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut fifo = loop {
                match OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&stat)
                {
                    Ok(fifo) => break fifo,
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("the caller's stat was never read: {e}"),
                }
            };
            let copy = fork_copy.then(|| holder.as_fd().try_clone_to_owned().unwrap());
            drop(holder);
            fifo.write_all(b"200 (bash) S 100 200 200 0 -1 4194560 0 0\n")
                .unwrap();
            copy
        });
        let (waiter_site, callers) = (site.clone(), callers_from(&proc, 300));
        let waited = within(Duration::from_secs(20), move || {
            wait_for(
                &waiter_site,
                "btrdasd walk",
                false,
                Some(&callers),
                &|| false,
                &NullProgress,
            )
        })
        .expect("never waits for a caller")
        .unwrap();
        // The copy, if any, lives until the waiter has decided.
        drop(caller.join().unwrap());
        (waited, crate::scrub::opens::count(&site.path))
    }

    /// A caller that lets go between the refused take and the reading of its
    /// record is not mistaken for a holder: the lock is taken, and kept.
    #[test]
    fn a_caller_that_lets_go_meanwhile_is_not_mistaken_for_the_holder() {
        let (waited, opens) = caller_lets_go_meanwhile(false);
        assert!(matches!(waited, Waited::Held(_)), "{waited:?}");
        assert_eq!(opens, 2, "refused once, then taken, and kept");
    }

    /// The same while a copy of the caller's descriptor lives on, as a fork
    /// of this process holds every descriptor until its exec — any test's
    /// spawn does (bd DAS-Backup-Manager-eu0). The caller let go of the lock,
    /// not merely of one descriptor on it: still not mistaken for the holder.
    #[test]
    fn a_caller_that_lets_go_while_a_fork_holds_a_copy_is_not_mistaken_for_the_holder() {
        let (waited, opens) = caller_lets_go_meanwhile(true);
        assert!(matches!(waited, Waited::Held(_)), "{waited:?}");
        assert_eq!(opens, 2, "refused once, then taken, and kept");
    }

    /// Spawns children in a tight loop until `stop`: `posix` as std spawns
    /// them (`posix_spawn`), `fork` through a `pre_exec` hook, which makes
    /// std fork and exec instead; `cpu` spins and spawns nothing. Either
    /// spawn copies this process's whole descriptor table until the child's
    /// exec. How many children it started.
    fn spawner(kind: &str, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<u64> {
        let kind = kind.to_string();
        std::thread::spawn(move || {
            use std::os::unix::process::CommandExt;
            use std::process::{Command, Stdio};
            let mut spawned = 0;
            while !stop.load(Ordering::Relaxed) {
                if kind == "cpu" {
                    std::hint::black_box((0..10_000u64).sum::<u64>());
                    continue;
                }
                let mut cmd = Command::new("true");
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if kind == "fork" {
                    // SAFETY: the hook does nothing at all, which is
                    // async-signal-safe.
                    unsafe { cmd.pre_exec(|| Ok(())) };
                }
                if cmd.status().is_ok() {
                    spawned += 1;
                }
            }
            spawned
        })
    }

    /// The flaky shape measured (bd DAS-Backup-Manager-eu0): the run above,
    /// `EU0_RUNS` times (default 1000), while threads of this process spawn
    /// children — `EU0_SPAWNERS`, a comma list of [`spawner`] kinds (default
    /// `posix,fork,posix,fork`; empty for none). Ignored: it measures, for
    /// seconds. `cargo test --lib -- --ignored --nocapture
    /// a_caller_letting_go_is_seen_to_let_go_while_this_process_spawns`
    #[test]
    #[ignore = "a measurement that runs for seconds; see its doc"]
    fn a_caller_letting_go_is_seen_to_let_go_while_this_process_spawns() {
        let runs: usize = std::env::var("EU0_RUNS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(1000);
        let kinds =
            std::env::var("EU0_SPAWNERS").unwrap_or_else(|_| "posix,fork,posix,fork".to_string());
        let stop = Arc::new(AtomicBool::new(false));
        let spawners: Vec<_> = kinds
            .split(',')
            .filter(|k| !k.is_empty())
            .map(|k| spawner(k, stop.clone()))
            .collect();
        let started = Instant::now();
        let mut mistaken = Vec::new();
        for _ in 0..runs {
            match caller_lets_go_meanwhile(false) {
                (Waited::Held(_), _) => {}
                (other, _) => mistaken.push(format!("{other:?}")),
            }
        }
        stop.store(true, Ordering::Relaxed);
        let spawned: u64 = spawners.into_iter().map(|t| t.join().unwrap()).sum();
        eprintln!(
            "eu0: {} of {runs} runs mistook the caller for the holder; spawners [{kinds}] \
             started {spawned} children; {:.1} s",
            mistaken.len(),
            started.elapsed().as_secs_f64()
        );
        assert!(
            mistaken.is_empty(),
            "{} of {runs}; the first: {}",
            mistaken.len(),
            mistaken[0]
        );
    }

    #[test]
    fn the_caller_is_told_to_hand_the_hold_down() {
        assert_eq!(
            caller_holds_line(200, "backup-run.sh pid 200"),
            "the DAS maintenance lock is held by my own caller (pid 200, backup-run.sh pid 200); \
             it must hand the hold down via DAS_MAINTENANCE_LOCK_FD"
        );
    }

    #[test]
    fn a_backup_or_scrub_that_waits_names_the_holder() {
        let (_dir, site) = scratch();
        let held = MaintenanceHeld::try_acquire_at(&site.path, "btrdasd restore browse")
            .unwrap()
            .unwrap();
        assert_eq!(
            blocked_line(&site.path),
            format!(
                "Waiting for the DAS maintenance lock {}, held by {}",
                site.path.display(),
                me("btrdasd restore browse")
            )
        );
        drop(held);
    }

    // --- a lock handed down ------------------------------------------------

    #[test]
    fn the_holders_own_descriptor_is_accepted_and_never_released_here() {
        let (_dir, site) = scratch();
        let parent = holding_descriptor(&site.path);
        let held = MaintenanceHeld::delegated_at(&site.path, parent.as_raw_fd())
            .expect("the holder's own descriptor proves the hold");
        assert_eq!(held.path(), site.path);
        drop(held);
        assert!(
            MaintenanceHeld::try_acquire_at(&site.path, "x")
                .unwrap()
                .is_none(),
            "dropping the proof must not release the lock the parent holds"
        );
        drop(parent);
        assert!(
            MaintenanceHeld::try_acquire_at(&site.path, "x")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_descriptor_that_does_not_hold_the_lock_is_refused() {
        let (_dir, site) = scratch();
        let holder = holding_descriptor(&site.path);
        let bystander = OpenOptions::new().read(true).open(&site.path).unwrap();
        let err = MaintenanceHeld::delegated_at(&site.path, bystander.as_raw_fd()).unwrap_err();
        assert!(err.contains("does not hold"), "{err}");
        drop(holder);
    }

    #[test]
    fn a_descriptor_on_another_file_is_refused() {
        let (dir, site) = scratch();
        std::fs::write(&site.path, "").unwrap();
        let elsewhere = holding_descriptor(&dir.path().join("other.lock"));
        let err = MaintenanceHeld::delegated_at(&site.path, elsewhere.as_raw_fd()).unwrap_err();
        assert!(err.contains("is not"), "{err}");
    }

    /// A free lock is nobody's hold: proving a hold must not take the lock
    /// through a descriptor that never held it, and must leave it free.
    #[test]
    fn a_descriptor_on_a_free_lock_is_refused_and_the_lock_stays_free() {
        let (_dir, site) = scratch();
        std::fs::write(&site.path, "").unwrap();
        // Opened, never locked: a descriptor that holds nothing.
        let bystander = OpenOptions::new().read(true).open(&site.path).unwrap();
        let err = MaintenanceHeld::delegated_at(&site.path, bystander.as_raw_fd()).unwrap_err();
        assert!(err.contains("does not hold"), "{err}");
        assert!(err.contains("the lock is free"), "{err}");
        assert!(
            MaintenanceHeld::try_acquire_at(&site.path, "x")
                .unwrap()
                .is_some(),
            "the lock must still be free for a fresh taker while that descriptor stays open"
        );
        drop(bystander);
    }

    #[test]
    fn a_descriptor_that_is_not_open_or_a_missing_lock_file_is_refused() {
        let (_dir, site) = scratch();
        let err = MaintenanceHeld::delegated_at(&site.path, 0).unwrap_err();
        assert!(err.contains("cannot stat"), "{err}");
        std::fs::write(&site.path, "").unwrap();
        let err = MaintenanceHeld::delegated_at(&site.path, -1).unwrap_err();
        assert!(err.contains("not an open descriptor"), "{err}");
    }
}
