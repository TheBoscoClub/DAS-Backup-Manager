/// Log level for progress messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

impl LogLevel {
    /// The level word the GUI's job log reads (`JobLog`'s `level`).
    pub fn word(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warning => "warn",
            LogLevel::Error => "error",
        }
    }
}

/// Callback trait for reporting progress from long-running operations.
/// Implementations must be Send + Sync for use across threads.
pub trait ProgressCallback: Send + Sync {
    /// A new stage has started (e.g., "Snapshotting", "Sending").
    fn on_stage(&self, stage: &str, total_steps: u64);

    /// Progress within the current stage.
    fn on_progress(&self, current: u64, total: u64, message: &str);

    /// Throughput update (bytes per second).
    fn on_throughput(&self, bytes_per_sec: u64);

    /// Log message at the given level.
    fn on_log(&self, level: LogLevel, message: &str);

    /// Operation completed.
    fn on_complete(&self, success: bool, summary: &str);
}

/// No-op implementation for when progress reporting isn't needed.
pub struct NullProgress;

impl ProgressCallback for NullProgress {
    fn on_stage(&self, _: &str, _: u64) {}
    fn on_progress(&self, _: u64, _: u64, _: &str) {}
    fn on_throughput(&self, _: u64) {}
    fn on_log(&self, _: LogLevel, _: &str) {}
    fn on_complete(&self, _: bool, _: &str) {}
}

/// One progress event, in the form a [`ProgressSink`] receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    Stage {
        stage: String,
        total_steps: u64,
    },
    Progress {
        current: u64,
        total: u64,
        message: String,
    },
    Log {
        level: LogLevel,
        message: String,
    },
    /// The job is over. [`OrderedProgress`] delivers exactly one, last.
    Finished {
        success: bool,
        summary: String,
    },
}

impl ProgressEvent {
    /// `current` as a whole percentage of `total`, capped at 100; 0 when
    /// `total` is 0 (nothing to measure against).
    pub fn percent(current: u64, total: u64) -> i32 {
        (u128::from(current) * 100)
            .checked_div(u128::from(total))
            .map_or(0, |p| p.min(100) as i32)
    }
}

/// Where an [`OrderedProgress`] delivers its events — for the D-Bus helper,
/// the signal emitter. Called from one thread only, one event at a time.
pub trait ProgressSink: Send + 'static {
    /// Every log line of the job, in order, cancelled or not: the record an
    /// operator reads afterwards (the helper's journal). Called before
    /// `emit` for the same line.
    fn journal(&mut self, level: LogLevel, message: &str);
    /// Delivered to the job's client. After `cancel`, only `Finished`.
    fn emit(&mut self, event: ProgressEvent);
}

/// A [`ProgressCallback`] that delivers every event to a sink in the order
/// the job produced it, and ends with exactly one [`ProgressEvent::Finished`].
///
/// The D-Bus helper used to spawn one task per event to emit its signal, so
/// lines a job wrote together could overtake each other on the way to the
/// GUI, and a job's finished signal could arrive before its last log lines
/// — or twice, once from the library's `on_complete` and once from the
/// helper (bd DAS-Backup-Manager-6bp). Here every event goes into one queue
/// and one thread drains it, so the order out is the order in.
///
/// `on_complete` does not end the job: the library calls it from inside a
/// step whose caller may still unmount, record and report. Its summary is
/// delivered as a log line, and only [`OrderedProgress::finish`] sends
/// `Finished`.
///
/// Cancellation (`cancel`) is a rule of this queue too: once a job is
/// cancelled its client has stopped listening for progress, so stage and
/// progress events are dropped and log lines are no longer sent to it — but
/// every log line still reaches `ProgressSink::journal`, because the work
/// goes on as root after a cancel and its record must not go silent, and the
/// end is never dropped.
/// `finish` still sends exactly one `Finished`, failed and saying the job was
/// cancelled, when the work really stops. Cancelling does not stop the work;
/// the caller decides whether it can.
pub struct OrderedProgress {
    /// Each event, and whether it still goes to the client.
    tx: std::sync::Mutex<Option<std::sync::mpsc::Sender<(ProgressEvent, bool)>>>,
    drain: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    cancelled: std::sync::atomic::AtomicBool,
}

impl OrderedProgress {
    pub fn new(mut sink: impl ProgressSink) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(ProgressEvent, bool)>();
        let drain = std::thread::spawn(move || {
            for (event, to_client) in rx {
                if let ProgressEvent::Log { level, message } = &event {
                    sink.journal(*level, message);
                }
                if to_client {
                    sink.emit(event);
                }
            }
        });
        Self {
            tx: std::sync::Mutex::new(Some(tx)),
            drain: std::sync::Mutex::new(Some(drain)),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The job's client cancelled it: stop sending it progress (log lines
    /// still reach the journal), and end it as cancelled when `finish` is
    /// called.
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    fn send(&self, event: ProgressEvent) {
        let to_client = !self.is_cancelled();
        // A cancelled job's log lines still go to the journal; its stage
        // and progress events go nowhere.
        if !to_client && !matches!(event, ProgressEvent::Log { .. }) {
            return;
        }
        let guard = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = guard.as_ref() {
            // Fails only when the drain thread is gone, i.e. the sink
            // panicked; there is nobody left to deliver to.
            let _ = tx.send((event, to_client));
        }
    }

    /// End the job: queue `Finished` behind everything already sent, then
    /// wait until the sink has received it. Later calls, and events sent
    /// after this, are dropped — the job has exactly one end.
    pub fn finish(&self, success: bool, summary: &str) {
        let tx = self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(tx) = tx else { return };
        let event = if self.is_cancelled() {
            ProgressEvent::Finished {
                success: false,
                summary: format!(
                    "cancelled — the job has stopped; it had got as far as: {summary}"
                ),
            }
        } else {
            ProgressEvent::Finished {
                success,
                summary: summary.to_owned(),
            }
        };
        let _ = tx.send((event, true));
        drop(tx);
        let drain = self.drain.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(drain) = drain
            && drain.join().is_err()
        {
            eprintln!("progress: the event sink panicked; some job events were not delivered");
        }
    }
}

impl ProgressCallback for OrderedProgress {
    fn on_stage(&self, stage: &str, total_steps: u64) {
        self.send(ProgressEvent::Stage {
            stage: stage.to_owned(),
            total_steps,
        });
    }

    fn on_progress(&self, current: u64, total: u64, message: &str) {
        self.send(ProgressEvent::Progress {
            current,
            total,
            message: message.to_owned(),
        });
    }

    fn on_throughput(&self, _bytes_per_sec: u64) {
        // No sink shows throughput on its own; it reaches the GUI inside
        // progress messages.
    }

    fn on_log(&self, level: LogLevel, message: &str) {
        self.send(ProgressEvent::Log {
            level,
            message: message.to_owned(),
        });
    }

    fn on_complete(&self, success: bool, summary: &str) {
        self.send(ProgressEvent::Log {
            level: if success {
                LogLevel::Info
            } else {
                LogLevel::Error
            },
            message: summary.to_owned(),
        });
    }
}

impl Drop for OrderedProgress {
    /// A job dropped without `finish` (it panicked) still delivers what it
    /// queued; no `Finished` is invented for it.
    fn drop(&mut self) {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(drain) = self.drain.lock().unwrap_or_else(|e| e.into_inner()).take()
            && drain.join().is_err()
        {
            eprintln!("progress: the event sink panicked; some job events were not delivered");
        }
    }
}

/// Collects progress events into vectors for testing.
#[cfg(test)]
pub struct TestProgress {
    pub stages: std::sync::Mutex<Vec<(String, u64)>>,
    pub logs: std::sync::Mutex<Vec<(LogLevel, String)>>,
    pub completed: std::sync::Mutex<Option<(bool, String)>>,
}

#[cfg(test)]
impl Default for TestProgress {
    fn default() -> Self {
        Self {
            stages: std::sync::Mutex::new(Vec::new()),
            logs: std::sync::Mutex::new(Vec::new()),
            completed: std::sync::Mutex::new(None),
        }
    }
}

#[cfg(test)]
impl TestProgress {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
impl ProgressCallback for TestProgress {
    fn on_stage(&self, stage: &str, total_steps: u64) {
        self.stages
            .lock()
            .unwrap()
            .push((stage.to_string(), total_steps));
    }

    fn on_progress(&self, _: u64, _: u64, _: &str) {}
    fn on_throughput(&self, _: u64) {}

    fn on_log(&self, level: LogLevel, message: &str) {
        self.logs.lock().unwrap().push((level, message.to_string()));
    }

    fn on_complete(&self, success: bool, summary: &str) {
        *self.completed.lock().unwrap() = Some((success, summary.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_progress_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NullProgress>();
    }

    #[test]
    fn test_progress_collects_events() {
        let tp = TestProgress::new();
        tp.on_stage("Snapshotting", 5);
        tp.on_log(LogLevel::Info, "Starting snapshot");
        tp.on_complete(true, "Done");

        let stages = tp.stages.lock().unwrap();
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].0, "Snapshotting");
        assert_eq!(stages[0].1, 5);

        let logs = tp.logs.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, LogLevel::Info);

        let completed = tp.completed.lock().unwrap();
        assert!(completed.as_ref().unwrap().0);
    }

    /// Records what a sink received. `delay` slows each `emit` by a time that
    /// SHRINKS with every event: with one task per event, a later event
    /// overtakes an earlier one, so the order out would no longer be the
    /// order in.
    struct Collect {
        got: std::sync::Arc<std::sync::Mutex<Vec<ProgressEvent>>>,
        journal: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        delay: Option<u64>,
    }

    impl ProgressSink for Collect {
        fn journal(&mut self, level: LogLevel, message: &str) {
            self.journal
                .lock()
                .unwrap()
                .push(format!("[{}] {message}", level.word()));
        }

        fn emit(&mut self, event: ProgressEvent) {
            if let Some(ms) = self.delay.as_mut() {
                std::thread::sleep(std::time::Duration::from_millis(*ms));
                *ms = ms.saturating_sub(1);
            }
            self.got.lock().unwrap().push(event);
        }
    }

    fn collecting(
        delay: Option<u64>,
    ) -> (
        OrderedProgress,
        std::sync::Arc<std::sync::Mutex<Vec<ProgressEvent>>>,
    ) {
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let progress = OrderedProgress::new(Collect {
            got: got.clone(),
            journal: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            delay,
        });
        (progress, got)
    }

    fn log(n: usize) -> ProgressEvent {
        ProgressEvent::Log {
            level: LogLevel::Info,
            message: format!("line {n}"),
        }
    }

    #[test]
    fn ordered_progress_delivers_events_in_the_order_they_were_made() {
        let (progress, got) = collecting(Some(20));
        for n in 0..20 {
            progress.on_log(LogLevel::Info, &format!("line {n}"));
        }
        progress.finish(true, "done");
        let got = got.lock().unwrap().clone();
        let mut want: Vec<ProgressEvent> = (0..20).map(log).collect();
        want.push(ProgressEvent::Finished {
            success: true,
            summary: "done".into(),
        });
        assert_eq!(got, want);
    }

    #[test]
    fn ordered_progress_keeps_every_kind_of_event_in_one_order() {
        let (progress, got) = collecting(Some(6));
        progress.on_stage("Mounting", 2);
        progress.on_progress(1, 2, "half");
        progress.on_log(LogLevel::Warning, "careful");
        progress.on_throughput(1234);
        progress.on_complete(false, "it went wrong");
        progress.finish(false, "Backup failed");
        assert_eq!(
            *got.lock().unwrap(),
            vec![
                ProgressEvent::Stage {
                    stage: "Mounting".into(),
                    total_steps: 2
                },
                ProgressEvent::Progress {
                    current: 1,
                    total: 2,
                    message: "half".into()
                },
                ProgressEvent::Log {
                    level: LogLevel::Warning,
                    message: "careful".into()
                },
                // on_complete is a log line, never a second end.
                ProgressEvent::Log {
                    level: LogLevel::Error,
                    message: "it went wrong".into()
                },
                ProgressEvent::Finished {
                    success: false,
                    summary: "Backup failed".into()
                },
            ]
        );
    }

    #[test]
    fn a_successful_on_complete_is_an_info_line() {
        let (progress, got) = collecting(None);
        progress.on_complete(true, "all good");
        progress.finish(true, "end");
        assert_eq!(
            got.lock().unwrap()[0],
            ProgressEvent::Log {
                level: LogLevel::Info,
                message: "all good".into()
            }
        );
    }

    #[test]
    fn finished_is_sent_exactly_once_and_last() {
        let (progress, got) = collecting(None);
        progress.on_log(LogLevel::Info, "before");
        progress.finish(true, "first");
        progress.finish(false, "second");
        progress.on_log(LogLevel::Info, "after");
        let got = got.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                log_msg("before"),
                ProgressEvent::Finished {
                    success: true,
                    summary: "first".into()
                }
            ]
        );
    }

    fn log_msg(m: &str) -> ProgressEvent {
        ProgressEvent::Log {
            level: LogLevel::Info,
            message: m.into(),
        }
    }

    #[test]
    fn finish_waits_until_the_sink_has_everything() {
        // A slow sink: finish must not return while events are still queued.
        let (progress, got) = collecting(Some(30));
        progress.on_log(LogLevel::Info, "slow");
        progress.finish(true, "end");
        assert_eq!(got.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_job_dropped_without_finish_delivers_its_events_and_no_finished() {
        let (progress, got) = collecting(Some(10));
        progress.on_log(LogLevel::Info, "partial");
        drop(progress);
        assert_eq!(*got.lock().unwrap(), vec![log_msg("partial")]);
    }

    #[test]
    fn percent_is_capped_and_zero_without_a_total() {
        assert_eq!(ProgressEvent::percent(1, 4), 25);
        assert_eq!(ProgressEvent::percent(4, 4), 100);
        assert_eq!(ProgressEvent::percent(9, 4), 100);
        assert_eq!(ProgressEvent::percent(3, 0), 0);
        assert_eq!(ProgressEvent::percent(0, 7), 0);
        assert_eq!(ProgressEvent::percent(u64::MAX, u64::MAX), 100);
    }

    #[test]
    fn level_words_are_what_the_gui_colours_by() {
        // gui/src/progresspanel.cpp matches "warn"/"warning" and "error".
        assert_eq!(LogLevel::Debug.word(), "debug");
        assert_eq!(LogLevel::Info.word(), "info");
        assert_eq!(LogLevel::Warning.word(), "warn");
        assert_eq!(LogLevel::Error.word(), "error");
    }

    #[test]
    fn a_cancelled_job_drops_its_progress_but_ends_exactly_once_as_cancelled() {
        let (progress, got) = collecting(None);
        progress.on_log(LogLevel::Info, "before");
        assert!(!progress.is_cancelled());
        progress.cancel();
        assert!(progress.is_cancelled());
        progress.on_log(LogLevel::Info, "after");
        progress.on_stage("Sending", 1);
        progress.on_progress(1, 1, "sent");
        progress.on_complete(true, "Backup succeeded");
        // The work reports success when it really stops; the client asked
        // for it to be cancelled, so it ends failed and says so.
        progress.finish(true, "Backup succeeded");
        progress.finish(true, "again");
        assert_eq!(
            *got.lock().unwrap(),
            vec![
                log_msg("before"),
                ProgressEvent::Finished {
                    success: false,
                    summary: "cancelled — the job has stopped; it had got as far as: \
                              Backup succeeded"
                        .into()
                },
            ]
        );
    }

    #[test]
    fn a_job_not_cancelled_ends_with_its_own_result() {
        let (progress, got) = collecting(None);
        progress.finish(false, "Mount failed");
        assert_eq!(
            *got.lock().unwrap(),
            vec![ProgressEvent::Finished {
                success: false,
                summary: "Mount failed".into()
            }]
        );
    }

    type Shared<T> = std::sync::Arc<std::sync::Mutex<Vec<T>>>;

    fn journalled() -> (OrderedProgress, Shared<ProgressEvent>, Shared<String>) {
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let journal = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let progress = OrderedProgress::new(Collect {
            got: got.clone(),
            journal: journal.clone(),
            delay: None,
        });
        (progress, got, journal)
    }

    #[test]
    fn every_log_line_reaches_the_journal_and_the_client_when_not_cancelled() {
        let (progress, got, journal) = journalled();
        progress.on_log(LogLevel::Warning, "one");
        progress.on_stage("Sending", 1);
        progress.on_log(LogLevel::Info, "two");
        progress.finish(true, "done");
        assert_eq!(*journal.lock().unwrap(), vec!["[warn] one", "[info] two"]);
        let got = got.lock().unwrap().clone();
        assert_eq!(got.len(), 4, "{got:?}");
    }

    /// After a cancel the work goes on as root: its log lines must still
    /// reach the journal, though the client hears only the end.
    #[test]
    fn a_cancelled_job_keeps_writing_to_the_journal_but_not_to_the_client() {
        let (progress, got, journal) = journalled();
        progress.on_log(LogLevel::Info, "before");
        progress.cancel();
        progress.on_log(LogLevel::Error, "after the cancel");
        progress.on_stage("Sending", 1);
        progress.on_complete(true, "Backup succeeded");
        progress.finish(true, "Backup succeeded");
        assert_eq!(
            *journal.lock().unwrap(),
            vec![
                "[info] before",
                "[error] after the cancel",
                "[info] Backup succeeded"
            ]
        );
        let got = got.lock().unwrap();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0], log_msg("before"));
        assert!(matches!(
            got[1],
            ProgressEvent::Finished { success: false, .. }
        ));
    }
}
