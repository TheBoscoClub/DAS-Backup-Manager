use crate::config::Config;

/// Information about the current backup schedule.
#[derive(Debug, Clone)]
pub struct ScheduleInfo {
    pub incremental_time: String,
    pub full_schedule: String,
    pub delay_min: u32,
    pub enabled: bool,
    pub next_incremental: Option<String>,
    pub next_full: Option<String>,
}

/// Get current schedule info by querying systemd timers (or cron).
pub fn get_schedule(_config: &Config) -> Result<ScheduleInfo, Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

/// Modify the backup schedule. Updates config AND regenerates timer/cron files.
pub fn set_schedule(
    _config: &mut Config,
    _incremental: Option<&str>,
    _full: Option<&str>,
    _delay: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

/// Enable or disable scheduled backups.
pub fn set_enabled(
    _config: &Config,
    _enabled: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_info_fields() {
        let info = ScheduleInfo {
            incremental_time: "03:00".into(),
            full_schedule: "Sun 04:00".into(),
            delay_min: 30,
            enabled: true,
            next_incremental: Some("2026-02-28 03:00".into()),
            next_full: None,
        };
        assert_eq!(info.incremental_time, "03:00");
        assert_eq!(info.delay_min, 30);
        assert!(info.enabled);
        assert!(info.next_incremental.is_some());
        assert!(info.next_full.is_none());
    }
}
