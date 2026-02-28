/// Log level for progress messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warning,
    Error,
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

/// Collects progress events into vectors for testing.
#[cfg(test)]
pub struct TestProgress {
    pub stages: std::sync::Mutex<Vec<(String, u64)>>,
    pub logs: std::sync::Mutex<Vec<(LogLevel, String)>>,
    pub completed: std::sync::Mutex<Option<(bool, String)>>,
}

#[cfg(test)]
impl TestProgress {
    pub fn new() -> Self {
        Self {
            stages: std::sync::Mutex::new(Vec::new()),
            logs: std::sync::Mutex::new(Vec::new()),
            completed: std::sync::Mutex::new(None),
        }
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
        self.logs
            .lock()
            .unwrap()
            .push((level, message.to_string()));
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
}
