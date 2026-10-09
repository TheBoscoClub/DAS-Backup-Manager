//! Running `recovery-os-vm.sh session …` as a job (bd 8249 stage 2).
//!
//! The D-Bus helper calls [`run_session`]: it validates the request, starts
//! the script in an environment built from nothing (no test seam of the
//! script's is reachable from the bus), turns the script's machine lines into
//! stages, percents and log lines, and returns the outcome the script's own
//! exit status gives. A cancel sends the script one SIGINT — the driver's
//! trap leaves a recovery OS it already started running and says so — and the
//! job then reads on to the script's real end.

use std::io::{self, BufRead, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::{Config, TargetRole};
use crate::progress::{self, LogLevel, ProgressCallback};
use crate::recovery_os::lines::{STEPS, SessionLine, parse_session_line, step_percent};

/// The installed driver; run by this absolute path, never found on `PATH`.
pub const SCRIPT: &str = "/usr/lib/das-backup/recovery-os-vm.sh";

/// The whole environment the script gets.
const SCRIPT_ENV: [(&str, &str); 3] = [
    ("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin"),
    ("LC_ALL", "C"),
    ("HOME", "/root"),
];

/// How often a silent script is checked for a cancel.
const CANCEL_TICK: Duration = Duration::from_millis(250);

/// Human lines the outcome keeps for its summary.
const LAST_LINES: usize = 12;

/// One session the helper was asked to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRequest {
    /// One or two `role = "mirror"` target labels.
    pub labels: Vec<String>,
    pub unattended: bool,
    /// `sequential` or `parallel`; only with two labels.
    pub mode: Option<String>,
    pub accept_boot_record_risk: bool,
}

impl SessionRequest {
    /// Refuses before anything runs: no label, more than two, a repeated one,
    /// a label the configuration does not list as role mirror, a mode with one
    /// label or an unknown mode.
    pub fn validate(&self, cfg: &Config) -> Result<(), String> {
        match self.labels.len() {
            0 => return Err("no drive given: name one or two recovery drives".into()),
            1 | 2 => {}
            n => return Err(format!("at most two drives per session ({n} given)")),
        }
        if self.labels.len() == 2 && self.labels[0] == self.labels[1] {
            return Err(format!("{} is named twice", self.labels[0]));
        }
        for label in &self.labels {
            match cfg.targets.iter().find(|t| &t.label == label) {
                None => return Err(format!("{label} is not in the configuration")),
                Some(t) if t.role != TargetRole::Mirror => {
                    return Err(format!(
                        "{label} is not a recovery drive (its role is not mirror)"
                    ));
                }
                Some(_) => {}
            }
        }
        if let Some(mode) = &self.mode {
            if self.labels.len() != 2 {
                return Err(format!("--mode {mode} needs two drives"));
            }
            if !matches!(mode.as_str(), "sequential" | "parallel") {
                return Err(format!("mode {mode}: must be sequential or parallel"));
            }
        }
        Ok(())
    }

    /// The script's argument vector:
    /// `session <l1> [<l2>] [--unattended] [--mode m] [--accept-boot-record-risk]`.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["session".to_string()];
        args.extend(self.labels.iter().cloned());
        if self.unattended {
            args.push("--unattended".into());
        }
        if let Some(mode) = &self.mode {
            args.push("--mode".into());
            args.push(mode.clone());
        }
        if self.accept_boot_record_risk {
            args.push("--accept-boot-record-risk".into());
        }
        args
    }
}

/// The command for `script args`, with an environment built, never
/// inherited: exactly [`SCRIPT_ENV`], so no `DAS_RECOVERY_*`, `DAS_CONFIG`,
/// `BTRDASD_BIN` or `DAS_RECOVERY_OS_STATE` seam reaches the script. Stdin is
/// closed; the working directory is `/`. The script leads a process group of
/// its own, so a cancel can reach it the way a terminal's Ctrl-C does.
pub fn script_command(script: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(script);
    cmd.args(args)
        .env_clear()
        .envs(SCRIPT_ENV)
        .current_dir("/")
        .stdin(Stdio::null())
        .process_group(0);
    cmd
}

/// How the job starts the script; the helper uses [`SystemSpawner`], tests
/// may wrap it.
pub trait SessionSpawner {
    fn spawn(&self, cmd: Command) -> io::Result<Box<dyn SessionChild>>;
}

/// A started script.
pub trait SessionChild {
    fn pid(&self) -> u32;
    /// Every stdout line as it arrives, and every stderr line prefixed
    /// `stderr: `, without the `\n` (or `\r\n`). The channel disconnects when
    /// both streams have ended. Handed out once.
    fn lines(&mut self) -> io::Result<mpsc::Receiver<String>>;
    /// Reap the script. An error reading its output is reported here, after
    /// the script is reaped.
    fn wait(&mut self) -> io::Result<ExitStatus>;
    /// SIGINT to the script's process group, as a terminal's Ctrl-C sends it.
    fn interrupt(&self) -> io::Result<()>;
}

/// Starts the script as a real child process.
pub struct SystemSpawner;

struct SystemChild {
    child: Child,
    lines: Option<mpsc::Receiver<String>>,
    readers: Vec<JoinHandle<io::Result<()>>>,
}

/// Send each line of `reader` (lossy UTF-8, `\n`/`\r\n` stripped) to `tx`
/// with `prefix`, until EOF. A receiver gone is not an error: the job stopped
/// listening, and the stream is still drained so the script never blocks.
fn pump(mut reader: impl BufRead, prefix: &str, tx: &mpsc::Sender<String>) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.ends_with(b"\n") {
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
        }
        let _ = tx.send(format!("{prefix}{}", String::from_utf8_lossy(&line)));
    }
}

fn start_reader(
    name: &str,
    stream: impl Read + Send + 'static,
    prefix: &'static str,
    tx: mpsc::Sender<String>,
) -> io::Result<JoinHandle<io::Result<()>>> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || pump(io::BufReader::new(stream), prefix, &tx))
}

impl SessionSpawner for SystemSpawner {
    fn spawn(&self, mut cmd: Command) -> io::Result<Box<dyn SessionChild>> {
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        // Both streams are drained from the start, each on its own thread: a
        // full stderr pipe would block the script, and with it stdout (the
        // hazard `fsutil::SystemRunner::stream` documents).
        let (tx, rx) = mpsc::channel();
        let mut readers = Vec::new();
        let streams = (child.stdout.take(), child.stderr.take());
        let started = match streams {
            (Some(out), Some(err)) => start_reader("session-stdout", out, "", tx.clone())
                .map(|h| readers.push(h))
                .and_then(|()| start_reader("session-stderr", err, "stderr: ", tx))
                .map(|h| readers.push(h)),
            _ => Err(io::Error::other("the script's output is not piped")),
        };
        if let Err(e) = started {
            // Never leave a started script behind unreaped.
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        Ok(Box::new(SystemChild {
            child,
            lines: Some(rx),
            readers,
        }))
    }
}

/// A pid that fits `pid_t` and is above 0. One that does not fit, or 0
/// (whose negation would signal our own process group), is refused rather
/// than converted.
fn signalable_pid(id: u32) -> io::Result<libc::pid_t> {
    libc::pid_t::try_from(id)
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| io::Error::other(format!("pid {id} cannot be signalled")))
}

impl SessionChild for SystemChild {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn lines(&mut self) -> io::Result<mpsc::Receiver<String>> {
        self.lines
            .take()
            .ok_or_else(|| io::Error::other("the script's lines were already handed out"))
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait()?;
        let mut failed = None;
        for reader in self.readers.drain(..) {
            match reader.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failed = Some(e.to_string()),
                Err(_) => failed = Some("a reader thread panicked".into()),
            }
        }
        match failed {
            None => Ok(status),
            Some(e) => Err(io::Error::other(format!(
                "reading the script's output failed ({e}); it ended with {status}"
            ))),
        }
    }

    fn interrupt(&self) -> io::Result<()> {
        let pid = signalable_pid(self.child.id())?;
        // The whole group, never the pid alone: the pair driver traps INT to
        // `:` and relies on its foreground drive session receiving the same
        // Ctrl-C, and bash defers a SIGINT its foreground child never got —
        // a pid-only signal let a two-drive run go on to drive 2, exit 0.
        // The script leads its group (`script_command`'s process_group(0)).
        // SAFETY: kill(2) takes plain integers; the child is not reaped yet
        // (wait() has not returned), so the group id is still the script's.
        if unsafe { libc::kill(-pid, libc::SIGINT) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// A child dropped before `wait()` reaped it — a progress callback that
/// panicked inside [`run_session`] unwinds past the wait — is reaped on a
/// detached thread, so the script never lingers as a zombie under the helper.
/// It is never killed: a session left running keeps its own guard and is
/// ended with `session-end`, as the script says.
impl Drop for SystemChild {
    fn drop(&mut self) {
        // `try_wait` reaps a child that already ended; `Ok(None)` is one still
        // running, `Err` one already reaped (nothing left to do either way).
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        let Ok(pid) = libc::pid_t::try_from(self.child.id()) else {
            return;
        };
        let reaper = std::thread::Builder::new()
            .name("session-reaper".into())
            .spawn(move || {
                let mut status = 0;
                // SAFETY: waitpid(2) on our own unreaped child; `Child` is
                // being dropped, so nothing else will wait for this pid.
                unsafe { libc::waitpid(pid, &mut status, 0) };
            });
        if let Err(e) = reaper {
            eprintln!("recovery-os session: pid {pid} left unreaped: no reaper thread: {e}");
        }
    }
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOutcome {
    /// The script's exit status; `None` when a signal killed it.
    pub exit: Option<i32>,
    /// The signal that killed it, when `exit` is `None`.
    pub signal: Option<i32>,
    /// Each `RESULT <label> <exit> <outcome>` line.
    pub results: Vec<(String, i32, String)>,
    /// Each `DRIVE <label> <exit|skipped>` line (`None`: skipped).
    pub drives: Vec<(String, Option<i32>)>,
    /// The last `PROGRESS … fail` line: step and message.
    pub failed_step: Option<(String, String)>,
    /// The last 12 human lines (not machine lines; stderr ones prefixed).
    pub last_lines: Vec<String>,
}

impl SessionOutcome {
    pub fn success(&self) -> bool {
        self.exit == Some(0)
    }

    /// `clean` | `warnings: …` | `kept (exit 3): …` | `failed (exit N): …` |
    /// `stopped at <step> (exit 7): …` | `killed by signal N: …`.
    pub fn summary(&self) -> String {
        let tail = self.last_lines.join("; ");
        let with = |head: String, detail: &str| {
            if detail.is_empty() {
                head
            } else {
                format!("{head}: {detail}")
            }
        };
        match self.exit {
            Some(0) => "clean".into(),
            Some(5) => with("warnings".into(), &tail),
            Some(3) => with("kept (exit 3)".into(), &tail),
            Some(7) => match &self.failed_step {
                Some((step, message)) => {
                    let detail = [message.as_str(), tail.as_str()]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .copied()
                        .collect::<Vec<_>>()
                        .join("; ");
                    with(format!("stopped at {step} (exit 7)"), &detail)
                }
                None => with("failed (exit 7)".into(), &tail),
            },
            Some(n) => with(format!("failed (exit {n})"), &tail),
            None => {
                let head = match self.signal {
                    Some(sig) => format!("killed by signal {sig}"),
                    None => "killed by signal".into(),
                };
                with(head, &tail)
            }
        }
    }
}

/// What one line of the script's output does to the job and its outcome.
fn take_line(line: &str, out: &mut SessionOutcome, progress: &dyn ProgressCallback) {
    match parse_session_line(line) {
        SessionLine::Progress {
            label,
            step,
            event,
            message,
        } => {
            // An unknown step is logged, never mapped to a stage.
            if event == "start" && step_percent(step).is_some() {
                progress.on_stage(&format!("{label}:{step}"), STEPS.len() as u64);
            }
            let text = if message.is_empty() {
                format!("[{label}] {step} {event}")
            } else {
                format!("[{label}] {step} {event}: {message}")
            };
            match step_percent(step) {
                Some(pct) => progress.on_progress(pct as u64, 100, &text),
                None => progress.on_log(LogLevel::Warning, &format!("unknown step: {line}")),
            }
            if event == "fail" {
                out.failed_step = Some((step.to_string(), message.to_string()));
                progress.on_log(LogLevel::Error, &text);
            }
        }
        SessionLine::Output { label, step, text } => {
            progress.on_log(LogLevel::Info, &format!("[{label}] {step}: {text}"));
        }
        SessionLine::Result {
            label,
            exit,
            outcome,
        } => {
            out.results
                .push((label.to_string(), exit, outcome.to_string()));
            let level = match exit {
                0 => LogLevel::Info,
                5 => LogLevel::Warning,
                _ => LogLevel::Error,
            };
            progress.on_log(level, &format!("[{label}] result: {outcome} (exit {exit})"));
        }
        SessionLine::Drive { label, exit } => {
            out.drives.push((label.to_string(), exit));
            let text = match exit {
                Some(n) => format!("[{label}] drive done (exit {n})"),
                None => format!("[{label}] drive skipped"),
            };
            progress.on_log(LogLevel::Info, &text);
        }
        SessionLine::Other(text) => {
            progress.on_log(LogLevel::Info, text);
            if out.last_lines.len() == LAST_LINES {
                out.last_lines.remove(0);
            }
            out.last_lines.push(text.to_string());
        }
    }
}

/// Runs the script to its end, mapping lines to progress: `PROGRESS` → stage
/// `<label>:<step>` (on `start`) and percent; `OUTPUT` → log `[<label>]
/// <step>: <text>`; `RESULT`/`DRIVE` → recorded and logged; anything else →
/// logged verbatim and kept for the summary. When a cancel is asked for —
/// checked on every line, and every 250 ms when none arrives — the script
/// gets one SIGINT and the job reads on to its end; the job counts as cut
/// short by the cancel only when the script then does not exit 0. The
/// outcome is the script's exit status, never the lines. `Err` only when the request is
/// refused or the script could not be run or reaped.
pub fn run_session(
    spawner: &dyn SessionSpawner,
    script: &Path,
    cfg: &Config,
    req: &SessionRequest,
    progress: &dyn ProgressCallback,
) -> Result<SessionOutcome, String> {
    req.validate(cfg)?;
    let mut child = spawner
        .spawn(script_command(script, &req.args()))
        .map_err(|e| format!("could not start {}: {e}", script.display()))?;
    let rx = match child.lines() {
        Ok(rx) => rx,
        Err(e) => {
            // Never leave it running unwatched: stop it and reap it.
            let _ = child.interrupt();
            let _ = child.wait();
            return Err(format!("could not read {}: {e}", script.display()));
        }
    };
    let mut out = SessionOutcome {
        exit: None,
        signal: None,
        results: Vec::new(),
        drives: Vec::new(),
        failed_step: None,
        last_lines: Vec::new(),
    };
    let mut interrupted = false;
    loop {
        match rx.recv_timeout(CANCEL_TICK) {
            Ok(line) => take_line(&line, &mut out, progress),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !interrupted && progress::cancel_requested(progress) {
            interrupted = true;
            match child.interrupt() {
                Ok(()) => progress.on_log(
                    LogLevel::Warning,
                    "cancel: sent SIGINT to the session script's process group; waiting for it to finish",
                ),
                Err(e) => progress.on_log(
                    LogLevel::Error,
                    &format!("cancel: could not signal the session script: {e}"),
                ),
            }
        }
    }
    let status = child
        .wait()
        .map_err(|e| format!("{}: {e}", script.display()))?;
    out.exit = status.code();
    out.signal = status.signal();
    // The cancel cut the job short only when the script did not end clean:
    // an exit 0 after the SIGINT is a cancel that came too late to stop
    // anything, and the job ends with that outcome.
    if interrupted && out.exit != Some(0) {
        progress::stop_requested(progress);
    }
    match (out.exit, out.signal) {
        (Some(0), _) => progress.on_log(LogLevel::Info, "session script exited 0"),
        (Some(n), _) => progress.on_log(LogLevel::Error, &format!("session script exited {n}")),
        (None, sig) => progress.on_log(
            LogLevel::Error,
            &format!("session script killed by signal {}", sig.unwrap_or(-1)),
        ),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{CancelToken, LogLevel, ProgressCallback};
    use crate::recovery_os::panel::tests::two_mirrors_and_a_primary;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An executable at `path`, written so no fd open for writing on it
    /// ever exists in this process. A file this (multi-threaded) test
    /// process wrote itself can be inherited, still open for writing, by a
    /// child another test is forking at that moment, and exec then fails with
    /// ETXTBSY (seen: 3 of 25 full-suite runs). The draft is written here;
    /// the executable is a fresh copy `install` makes in its own process.
    fn write_executable(path: &Path, content: &str) {
        let draft = path.with_extension("draft");
        std::fs::write(&draft, content).unwrap();
        let status = Command::new("install")
            .arg("-m")
            .arg("755")
            .arg(&draft)
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "install {} failed", path.display());
    }

    fn stub_script(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("recovery-os-vm.sh");
        write_executable(&p, &format!("#!/bin/bash\n{body}\n"));
        p
    }

    fn one_drive() -> SessionRequest {
        SessionRequest {
            labels: vec!["system-recovery-A-2tb".into()],
            unattended: false,
            mode: None,
            accept_boot_record_risk: false,
        }
    }

    /// Ends the whole test process if its owner outlives 12 s. Every session
    /// test builds one (through `CapturingProgress`), so a read loop that
    /// never sees its stream end — a mutated `pump` spins forever on EOF, and
    /// `wait()` then joins it — fails the run, instead of hanging it until
    /// the harness gives up. Normal tests take well under a second.
    struct Watchdog(Option<mpsc::Sender<()>>);

    impl Default for Watchdog {
        fn default() -> Self {
            let (tx, rx) = mpsc::channel::<()>();
            let name = std::thread::current().name().unwrap_or("?").to_string();
            std::thread::spawn(move || {
                if rx.recv_timeout(Duration::from_secs(12)) == Err(mpsc::RecvTimeoutError::Timeout)
                {
                    // Straight to stderr (`eprintln!` is captured per test and
                    // would be lost) and `_exit`, not abort: no core dump to
                    // delay the exit past a harness timeout.
                    let _ = io::Write::write_all(
                        &mut io::stderr(),
                        format!("watchdog: {name} still running after 12 s; ending the run\n")
                            .as_bytes(),
                    );
                    // SAFETY: _exit(2) takes a plain integer and never returns.
                    unsafe { libc::_exit(101) };
                }
            });
            Watchdog(Some(tx))
        }
    }

    impl Drop for Watchdog {
        fn drop(&mut self) {
            self.0.take();
        }
    }

    /// Records stages and logs; with `cancel_after_first_stage`, its cancel
    /// token is set by the first `on_stage`.
    #[derive(Default)]
    struct CapturingProgress {
        stages: Mutex<Vec<String>>,
        logs: Mutex<Vec<String>>,
        levelled: Mutex<Vec<(LogLevel, String)>>,
        _watchdog: Watchdog,
        cancel: CancelToken,
        cancel_on_stage: std::sync::atomic::AtomicBool,
    }

    impl CapturingProgress {
        fn cancel_after_first_stage(&self) {
            self.cancel_on_stage.store(true, Ordering::SeqCst);
        }
        fn stages(&self) -> Vec<String> {
            self.stages.lock().unwrap().clone()
        }
        fn logs(&self) -> Vec<String> {
            self.logs.lock().unwrap().clone()
        }
        fn level_of(&self, message: &str) -> Option<LogLevel> {
            self.levelled
                .lock()
                .unwrap()
                .iter()
                .find(|(_, m)| m == message)
                .map(|(l, _)| *l)
        }
    }

    impl ProgressCallback for CapturingProgress {
        fn on_stage(&self, stage: &str, _: u64) {
            self.stages.lock().unwrap().push(stage.to_string());
            if self.cancel_on_stage.load(Ordering::SeqCst) {
                self.cancel.cancel();
            }
        }
        fn on_progress(&self, _: u64, _: u64, _: &str) {}
        fn on_throughput(&self, _: u64) {}
        fn on_log(&self, level: LogLevel, message: &str) {
            self.levelled
                .lock()
                .unwrap()
                .push((level, message.to_string()));
            self.logs.lock().unwrap().push(message.to_string());
        }
        fn on_complete(&self, _: bool, _: &str) {}
        fn cancel_token(&self) -> Option<&CancelToken> {
            Some(&self.cancel)
        }
    }

    /// `SystemSpawner`, counting the interrupts its child is sent.
    #[derive(Default)]
    struct CountingSpawner {
        interrupts: std::sync::Arc<AtomicUsize>,
    }

    struct CountingChild {
        inner: Box<dyn SessionChild>,
        interrupts: std::sync::Arc<AtomicUsize>,
    }

    impl SessionSpawner for CountingSpawner {
        fn spawn(&self, cmd: Command) -> io::Result<Box<dyn SessionChild>> {
            Ok(Box::new(CountingChild {
                inner: SystemSpawner.spawn(cmd)?,
                interrupts: self.interrupts.clone(),
            }))
        }
    }

    impl SessionChild for CountingChild {
        fn pid(&self) -> u32 {
            self.inner.pid()
        }
        fn lines(&mut self) -> io::Result<mpsc::Receiver<String>> {
            self.inner.lines()
        }
        fn wait(&mut self) -> io::Result<ExitStatus> {
            self.inner.wait()
        }
        fn interrupt(&self) -> io::Result<()> {
            self.interrupts.fetch_add(1, Ordering::SeqCst);
            self.inner.interrupt()
        }
    }

    #[test]
    fn a_request_is_validated_before_anything_runs() {
        let cfg = two_mirrors_and_a_primary();
        let ok = one_drive();
        assert_eq!(ok.validate(&cfg), Ok(()));
        let bad = |labels: &[&str], mode: Option<&str>| {
            SessionRequest {
                labels: labels.iter().map(|s| s.to_string()).collect(),
                unattended: true,
                mode: mode.map(String::from),
                accept_boot_record_risk: false,
            }
            .validate(&cfg)
            .unwrap_err()
        };
        assert!(bad(&[], None).contains("no drive"));
        assert!(bad(&["primary-22tb"], None).contains("not a recovery drive"));
        assert!(bad(&["nope"], None).contains("not in the configuration"));
        assert!(bad(&["system-recovery-A-2tb", "system-recovery-A-2tb"], None).contains("twice"));
        assert!(
            bad(
                &[
                    "system-recovery-A-2tb",
                    "system-recovery-B-2tb",
                    "system-recovery-A-2tb"
                ],
                None
            )
            .contains("at most two")
        );
        assert!(bad(&["system-recovery-A-2tb"], Some("parallel")).contains("two drives"));
        assert!(
            bad(
                &["system-recovery-A-2tb", "system-recovery-B-2tb"],
                Some("fast")
            )
            .contains("sequential or parallel")
        );
        assert_eq!(
            SessionRequest {
                labels: vec![
                    "system-recovery-A-2tb".into(),
                    "system-recovery-B-2tb".into()
                ],
                unattended: true,
                mode: Some("parallel".into()),
                accept_boot_record_risk: true
            }
            .args(),
            [
                "session",
                "system-recovery-A-2tb",
                "system-recovery-B-2tb",
                "--unattended",
                "--mode",
                "parallel",
                "--accept-boot-record-risk"
            ]
        );
        assert_eq!(one_drive().args(), ["session", "system-recovery-A-2tb"]);
    }

    #[test]
    fn an_invalid_request_never_starts_the_script() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let script = stub_script(dir.path(), &format!("touch {}", marker.display()));
        let req = SessionRequest {
            labels: vec!["primary-22tb".into()],
            ..one_drive()
        };
        let err = run_session(
            &SystemSpawner,
            &script,
            &two_mirrors_and_a_primary(),
            &req,
            &CapturingProgress::default(),
        )
        .unwrap_err();
        assert!(err.contains("not a recovery drive"), "{err}");
        assert!(!marker.exists(), "the script ran for a refused request");
    }

    #[test]
    fn the_script_runs_in_a_built_environment_with_no_seam_reachable() {
        const POISON: [(&str, &str); 4] = [
            ("DAS_RECOVERY_VM_TEST_ROOT", "/tmp/poison"),
            ("DAS_RECOVERY_OS_STATE", "/tmp/poison.json"),
            ("DAS_CONFIG", "/tmp/poison.toml"),
            ("BTRDASD_BIN", "/tmp/poison-bin"),
        ];
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let script = stub_script(dir.path(), "env | sort; exit 0");
        let previous: Vec<_> = POISON.iter().map(|(k, _)| std::env::var_os(k)).collect();
        // Poison the parent's environment the way a misconfigured unit might.
        // SAFETY: only this test mutates these variables, under ENV_LOCK; the
        // one library reader (`recovery_os::state_path`) is given a path that
        // does not exist, which it already handles.
        unsafe {
            for (k, v) in POISON {
                std::env::set_var(k, v);
            }
        }
        let sink = CapturingProgress::default();
        let out = run_session(
            &SystemSpawner,
            &script,
            &two_mirrors_and_a_primary(),
            &one_drive(),
            &sink,
        );
        unsafe {
            for ((k, _), prev) in POISON.iter().zip(previous) {
                match prev {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        let out = out.unwrap();
        assert_eq!(out.exit, Some(0));
        let env = sink.logs().join("\n");
        for v in ["DAS_RECOVERY", "DAS_CONFIG", "BTRDASD_BIN"] {
            assert!(!env.contains(v), "{v} reached the script:\n{env}");
        }
        assert!(
            env.contains("LC_ALL=C")
                && env.contains("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin")
                && env.contains("HOME=/root"),
            "{env}"
        );
    }

    #[test]
    fn lines_become_stages_logs_results_and_the_exit_is_the_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let script = stub_script(
            dir.path(),
            r#"
echo "took the DAS maintenance lock"
echo "PROGRESS system-recovery-A-2tb preflight ok lock taken; unattended, single"
echo "PROGRESS system-recovery-A-2tb upgrade start"
echo "OUTPUT system-recovery-A-2tb upgrade :: Starting full system upgrade..."
printf 'a CRLF line\r\n'
echo "a line on stderr" >&2
echo "DRIVE system-recovery-A-2tb skipped"
echo "PROGRESS system-recovery-A-2tb upgrade fail pacman exited 1"
echo "RESULT system-recovery-A-2tb 7 failed"
exit 7"#,
        );
        let sink = CapturingProgress::default();
        let out = run_session(
            &SystemSpawner,
            &script,
            &two_mirrors_and_a_primary(),
            &one_drive(),
            &sink,
        )
        .unwrap();
        assert_eq!(out.exit, Some(7));
        assert_eq!(
            out.results,
            vec![("system-recovery-A-2tb".to_string(), 7, "failed".to_string())]
        );
        assert_eq!(
            out.drives,
            vec![("system-recovery-A-2tb".to_string(), None)]
        );
        assert!(
            sink.stages()
                .contains(&"system-recovery-A-2tb:upgrade".to_string())
        );
        let logs = sink.logs();
        assert!(
            logs.iter()
                .any(|l| l.contains(":: Starting full system upgrade"))
        );
        assert!(
            logs.iter().any(|l| l == "took the DAS maintenance lock"),
            "human lines are logged, not lost"
        );
        assert!(logs.iter().any(|l| l == "a CRLF line"), "{logs:?}");
        assert!(
            logs.iter().any(|l| l == "stderr: a line on stderr"),
            "{logs:?}"
        );
        assert!(!out.success());
        let s = out.summary();
        assert!(s.starts_with("stopped at upgrade (exit 7)"), "{s}");
        assert!(s.contains("pacman exited 1"));
    }

    #[test]
    fn exit_5_is_not_success_and_the_summary_begins_with_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let run = |body: &str| {
            let script = stub_script(dir.path(), body);
            run_session(
                &SystemSpawner,
                &script,
                &two_mirrors_and_a_primary(),
                &one_drive(),
                &CapturingProgress::default(),
            )
            .unwrap()
        };
        let out = run(
            "echo 'RESULT system-recovery-A-2tb 5 warnings'; echo 'WARNING: the claim was lost while the VM ran'; exit 5",
        );
        assert!(!out.success());
        assert!(out.summary().starts_with("warnings"), "{}", out.summary());
        assert!(out.summary().contains("claim was lost"));
        let out = run("echo 'RESULT system-recovery-A-2tb 0 clean'; exit 0");
        assert!(out.success());
        assert_eq!(out.summary(), "clean");
        let out = run("echo 'RESULT system-recovery-A-2tb 3 kept'; exit 3");
        assert!(
            out.summary().starts_with("kept (exit 3)"),
            "{}",
            out.summary()
        );
        let out = run("echo 'something broke'; exit 2");
        assert!(
            out.summary()
                .starts_with("failed (exit 2): something broke"),
            "{}",
            out.summary()
        );
        // A clean RESULT line does not make a failing exit a success.
        let out = run("echo 'RESULT system-recovery-A-2tb 0 clean'; exit 1");
        assert!(!out.success());
        assert!(
            out.summary().starts_with("failed (exit 1)"),
            "{}",
            out.summary()
        );
    }

    #[test]
    fn a_script_killed_by_a_signal_has_no_exit_and_is_not_success() {
        let dir = tempfile::tempdir().unwrap();
        let script = stub_script(dir.path(), "echo 'going down'; kill -KILL $$");
        let out = run_session(
            &SystemSpawner,
            &script,
            &two_mirrors_and_a_primary(),
            &one_drive(),
            &CapturingProgress::default(),
        )
        .unwrap();
        assert_eq!(out.exit, None);
        assert!(!out.success());
        let s = out.summary();
        assert!(s.starts_with("killed by signal"), "{s}");
        assert!(s.contains("9"), "{s}");
    }

    #[test]
    fn a_stop_request_interrupts_once_and_the_job_ends_with_the_scripts_exit() {
        let dir = tempfile::tempdir().unwrap();
        // The stub traps INT like the driver: says so, exits 3 (VM left running).
        let script = stub_script(
            dir.path(),
            r#"
trap 'echo "interrupted: the recovery OS is still running; finish with session-end"; exit 3' INT
echo "PROGRESS system-recovery-A-2tb wait start"
for i in $(seq 1 100); do sleep 0.1; done
exit 0"#,
        );
        let sink = CapturingProgress::default();
        sink.cancel_after_first_stage();
        let spawner = CountingSpawner::default();
        let out = run_session(
            &spawner,
            &script,
            &two_mirrors_and_a_primary(),
            &one_drive(),
            &sink,
        )
        .unwrap();
        assert_eq!(out.exit, Some(3));
        assert!(out.summary().contains("session-end"), "{}", out.summary());
        assert_eq!(spawner.interrupts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_cancel_is_honoured_only_when_the_script_did_not_end_clean() {
        // The script was already done: it takes the SIGINT and exits 0. The
        // cancel came too late — never a job that a cancel cut short.
        let dir = tempfile::tempdir().unwrap();
        let script = stub_script(
            dir.path(),
            r#"
trap 'echo "nothing left to stop"; exit 0' INT
echo "PROGRESS system-recovery-A-2tb wait start"
for i in $(seq 1 100); do sleep 0.1; done
exit 0"#,
        );
        let sink = CapturingProgress::default();
        sink.cancel_after_first_stage();
        let spawner = CountingSpawner::default();
        let out = run_session(
            &spawner,
            &script,
            &two_mirrors_and_a_primary(),
            &one_drive(),
            &sink,
        )
        .unwrap();
        assert_eq!(spawner.interrupts.load(Ordering::SeqCst), 1);
        assert_eq!(out.exit, Some(0));
        assert!(sink.cancel.is_requested());
        assert!(!sink.cancel.honoured(), "an exit 0 is not cut short");
        // The script stopped short (exit 3): the cancel was acted on.
        let script = stub_script(
            dir.path(),
            r#"
trap 'echo "interrupted"; exit 3' INT
echo "PROGRESS system-recovery-A-2tb wait start"
for i in $(seq 1 100); do sleep 0.1; done
exit 0"#,
        );
        let sink = CapturingProgress::default();
        sink.cancel_after_first_stage();
        let out = run_session(
            &CountingSpawner::default(),
            &script,
            &two_mirrors_and_a_primary(),
            &one_drive(),
            &sink,
        )
        .unwrap();
        assert_eq!(out.exit, Some(3));
        assert!(sink.cancel.honoured());
    }

    #[test]
    fn an_unknown_progress_step_is_never_a_stage() {
        let dir = tempfile::tempdir().unwrap();
        let script = stub_script(
            dir.path(),
            r#"
echo "PROGRESS system-recovery-A-2tb frobnicate start"
echo "PROGRESS system-recovery-A-2tb wait start"
exit 0"#,
        );
        let sink = CapturingProgress::default();
        run_session(
            &SystemSpawner,
            &script,
            &two_mirrors_and_a_primary(),
            &one_drive(),
            &sink,
        )
        .unwrap();
        assert_eq!(sink.stages(), ["system-recovery-A-2tb:wait"]);
        assert_eq!(
            sink.level_of("unknown step: PROGRESS system-recovery-A-2tb frobnicate start"),
            Some(LogLevel::Warning)
        );
    }

    #[test]
    fn a_stop_request_reaches_the_drive_session_running_in_the_foreground() {
        let dir = tempfile::tempdir().unwrap();
        // The pair driver ignores INT itself and relies on its foreground
        // drive session getting the terminal's Ctrl-C (the whole group).
        let drive = dir.path().join("drive.sh");
        write_executable(
            &drive,
            r#"#!/bin/bash
trap 'echo "interrupted: drive 1 left running"; exit 3' INT
echo "PROGRESS system-recovery-A-2tb wait start"
for i in $(seq 1 30); do sleep 0.1; done
exit 0
"#,
        );
        let script = stub_script(
            dir.path(),
            &format!(
                r#"
trap ':' INT TERM HUP
{}
rc=$?
[ "$rc" -eq 0 ] || exit "$rc"
echo "drive 2"
exit 0"#,
                drive.display()
            ),
        );
        let sink = CapturingProgress::default();
        sink.cancel_after_first_stage();
        let spawner = CountingSpawner::default();
        let out = run_session(
            &spawner,
            &script,
            &two_mirrors_and_a_primary(),
            &one_drive(),
            &sink,
        )
        .unwrap();
        let logs = sink.logs();
        assert!(!logs.iter().any(|l| l == "drive 2"), "{logs:?}");
        assert_eq!(out.exit, Some(3), "{logs:?}");
        assert_eq!(spawner.interrupts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_stop_request_is_seen_without_a_line_arriving() {
        let dir = tempfile::tempdir().unwrap();
        // Silent after its one stage: only the tick can see the cancel.
        let script = stub_script(
            dir.path(),
            r#"
trap 'echo "interrupted"; exit 3' INT
echo "PROGRESS system-recovery-A-2tb wait start"
for i in $(seq 1 100); do sleep 0.1; done
exit 0"#,
        );
        let sink = CapturingProgress::default();
        let spawner = CountingSpawner::default();
        let started = std::time::Instant::now();
        let out = std::thread::scope(|s| {
            s.spawn(|| {
                // After the script's first line (its trap is set by then),
                // never on a fixed clock that a slow start could beat.
                // Bounded: a run that never records the stage must fail
                // this test, not hang it (a mutated parser did, as a timeout).
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while sink.stages().is_empty() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "no stage was recorded within 10 s"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
                sink.cancel.cancel();
            });
            run_session(
                &spawner,
                &script,
                &two_mirrors_and_a_primary(),
                &one_drive(),
                &sink,
            )
            .unwrap()
        });
        assert_eq!(out.exit, Some(3));
        assert_eq!(spawner.interrupts.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_result_line_is_logged_at_the_level_its_exit_status_earns() {
        let dir = tempfile::tempdir().unwrap();
        let script = stub_script(
            dir.path(),
            r#"
echo "RESULT system-recovery-A-2tb 0 clean"
echo "RESULT system-recovery-B-2tb 5 warnings"
echo "RESULT system-recovery-C-2tb 7 failed"
exit 7"#,
        );
        let sink = CapturingProgress::default();
        run_session(
            &SystemSpawner,
            &script,
            &two_mirrors_and_a_primary(),
            &one_drive(),
            &sink,
        )
        .unwrap();
        assert_eq!(
            sink.level_of("[system-recovery-A-2tb] result: clean (exit 0)"),
            Some(LogLevel::Info)
        );
        assert_eq!(
            sink.level_of("[system-recovery-B-2tb] result: warnings (exit 5)"),
            Some(LogLevel::Warning)
        );
        assert_eq!(
            sink.level_of("[system-recovery-C-2tb] result: failed (exit 7)"),
            Some(LogLevel::Error)
        );
    }

    #[test]
    fn a_pid_is_signalled_only_when_it_fits_and_is_above_zero() {
        assert!(signalable_pid(0).is_err(), "0 would signal our own group");
        assert_eq!(signalable_pid(1).unwrap(), 1);
        assert_eq!(signalable_pid(4_000_000).unwrap(), 4_000_000);
        assert!(signalable_pid(u32::MAX).is_err());
        assert!(
            signalable_pid(0).unwrap_err().to_string().contains("pid 0"),
            "the refusal names the pid"
        );
    }

    #[test]
    fn pump_sends_every_line_stripped_and_prefixed_and_stops_at_the_end() {
        // Each run is on its own thread with a deadline: a pump that never
        // sees the end of its input spins forever, and must fail here
        // rather than hang the suite.
        let run = |input: &'static [u8]| {
            let (done_tx, done_rx) = mpsc::channel();
            std::thread::spawn(move || {
                let (tx, rx) = mpsc::channel();
                let result = pump(io::Cursor::new(input), "p: ", &tx);
                drop(tx);
                let _ = done_tx.send((result.is_ok(), rx.iter().collect::<Vec<_>>()));
            });
            done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("pump did not return at the end of its input")
        };
        assert_eq!(
            run(b"one\r\ntwo\nlast"),
            (
                true,
                vec!["p: one".to_string(), "p: two".into(), "p: last".into()]
            )
        );
        assert_eq!(run(b""), (true, Vec::new()));
        assert_eq!(
            run(b"bad \xff byte\n"),
            (true, vec!["p: bad \u{fffd} byte".to_string()])
        );
    }

    #[test]
    fn interrupt_reports_what_kill_reported() {
        use std::os::unix::process::ExitStatusExt;
        let _watchdog = Watchdog::default();
        // A live script leading its own group: the signal is delivered.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exec sleep 5"]).process_group(0);
        let mut child = SystemSpawner.spawn(cmd).unwrap();
        child.interrupt().unwrap();
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}");
        // The same child once reaped: its group is gone, kill says so.
        let err = child.interrupt().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ESRCH), "{err}");
    }

    /// `/proc/<pid>/stat`'s state letter, or `None` once the pid is gone.
    fn proc_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?.1.trim_start().chars().next()
    }

    fn wait_for_state(pid: u32, want: impl Fn(Option<char>) -> bool) -> bool {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < until {
            if want(proc_state(pid)) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn a_child_dropped_unwaited_is_reaped_never_left_a_zombie() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"]);
        let child = SystemSpawner.spawn(cmd).unwrap();
        let pid = child.pid();
        assert!(
            wait_for_state(pid, |s| s == Some('Z')),
            "the child never exited"
        );
        drop(child);
        assert!(
            wait_for_state(pid, |s| s.is_none()),
            "pid {pid} left a zombie after its SessionChild was dropped"
        );
    }

    #[test]
    fn dropping_a_running_child_never_kills_it() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 0.5"]);
        let child = SystemSpawner.spawn(cmd).unwrap();
        let pid = child.pid();
        drop(child);
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            matches!(proc_state(pid), Some(c) if c != 'Z'),
            "the dropped child stopped running: {:?}",
            proc_state(pid)
        );
        assert!(
            wait_for_state(pid, |s| s.is_none()),
            "pid {pid} was not reaped once it ended"
        );
    }

    #[test]
    fn a_script_that_cannot_start_is_an_error_not_an_outcome() {
        let err = run_session(
            &SystemSpawner,
            Path::new("/nonexistent/recovery-os-vm.sh"),
            &two_mirrors_and_a_primary(),
            &one_drive(),
            &CapturingProgress::default(),
        )
        .unwrap_err();
        assert!(err.contains("/nonexistent/recovery-os-vm.sh"));
    }
}
