use crate::config::Config;

/// Overall health status of the backup system.
#[derive(Debug, Clone, PartialEq)]
pub enum HealthStatus {
    Healthy,
    Warning,
    Critical,
}

/// Health information for a single target drive.
#[derive(Debug, Clone)]
pub struct TargetHealth {
    pub label: String,
    pub serial: String,
    pub mounted: bool,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub snapshot_count: usize,
    pub smart_status: Option<String>,
}

impl TargetHealth {
    /// Percentage of disk space used (0.0-100.0).
    pub fn usage_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.used_bytes as f64 / self.total_bytes as f64) * 100.0
    }
}

/// Growth trend data point.
#[derive(Debug, Clone)]
pub struct GrowthPoint {
    pub timestamp: i64,
    pub target_label: String,
    pub used_bytes: u64,
}

/// Full health report for the backup system.
#[derive(Debug)]
pub struct HealthReport {
    pub status: HealthStatus,
    pub targets: Vec<TargetHealth>,
    pub last_backup: Option<String>,
    pub growth_points: Vec<GrowthPoint>,
    pub warnings: Vec<String>,
}

/// Query health status of all configured targets.
pub fn get_health(_config: &Config) -> Result<HealthReport, Box<dyn std::error::Error>> {
    todo!("Full implementation in Phase 2")
}

/// Parse a growth.log file into GrowthPoint entries.
pub fn parse_growth_log(content: &str) -> Vec<GrowthPoint> {
    let mut points = Vec::new();
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3
            && let (Ok(ts), Ok(used)) = (parts[0].parse::<i64>(), parts[2].parse::<u64>())
        {
            points.push(GrowthPoint {
                timestamp: ts,
                target_label: parts[1].to_string(),
                used_bytes: used,
            });
        }
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_health_usage_percent() {
        let th = TargetHealth {
            label: "test".into(),
            serial: "ABC".into(),
            mounted: true,
            total_bytes: 1_000_000,
            used_bytes: 250_000,
            snapshot_count: 10,
            smart_status: Some("PASSED".into()),
        };
        let pct = th.usage_percent();
        assert!((pct - 25.0).abs() < 0.01);
    }

    #[test]
    fn target_health_usage_percent_zero_total() {
        let th = TargetHealth {
            label: "empty".into(),
            serial: "X".into(),
            mounted: false,
            total_bytes: 0,
            used_bytes: 0,
            snapshot_count: 0,
            smart_status: None,
        };
        assert_eq!(th.usage_percent(), 0.0);
    }

    #[test]
    fn parse_growth_log_entries() {
        let log = "1709000000 primary-22tb 5368709120\n\
                    1709086400 primary-22tb 5905580032\n\
                    1709000000 system-2tb 1073741824\n";
        let points = parse_growth_log(log);
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].timestamp, 1709000000);
        assert_eq!(points[0].target_label, "primary-22tb");
        assert_eq!(points[0].used_bytes, 5368709120);
        assert_eq!(points[2].target_label, "system-2tb");
    }

    #[test]
    fn parse_growth_log_skips_malformed() {
        let log = "bad line\n1709000000 ok 100\nincomplete 42\n";
        let points = parse_growth_log(log);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].target_label, "ok");
    }

    #[test]
    fn health_status_equality() {
        assert_eq!(HealthStatus::Healthy, HealthStatus::Healthy);
        assert_ne!(HealthStatus::Healthy, HealthStatus::Warning);
        assert_ne!(HealthStatus::Warning, HealthStatus::Critical);
    }
}
