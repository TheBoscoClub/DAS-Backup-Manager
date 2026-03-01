//! C-ABI FFI module for the buttered_dasd library.
//!
//! Provides an opaque-pointer based C API for config loading, subvolume listing,
//! health log parsing, database access, and utility functions. All returned
//! `*mut c_char` strings must be freed by the caller via [`btrdasd_string_free`].
//!
//! Gated behind the `ffi` feature flag.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::ptr;

use crate::config::Config;
use crate::db::Database;
use crate::health::parse_growth_log;
use crate::report::format_bytes;
use crate::subvol::list_subvolumes;

// ---------------------------------------------------------------------------
// Opaque handle types
// ---------------------------------------------------------------------------

/// Opaque handle to a loaded [`Config`]. Internally a `Box<Config>`.
type ConfigHandle = *mut c_void;

/// Opaque handle to an open [`Database`]. Internally a `Box<Database>`.
type DbHandle = *mut c_void;

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a Rust `String` to a heap-allocated C string.
/// Returns null on interior NUL bytes (should never happen with our data).
fn string_to_c(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(cs) => cs.into_raw(),
        Err(_) => ptr::null_mut(),
    }
}

/// Safely convert a `*const c_char` to a `&str`. Returns `None` on null or
/// invalid UTF-8.
unsafe fn cstr_to_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

// ---------------------------------------------------------------------------
// Config functions
// ---------------------------------------------------------------------------

/// Load a config from the TOML file at `path`.
///
/// Returns a [`ConfigHandle`] on success, or null on error (file not found,
/// parse failure, null `path`).
///
/// # Safety
///
/// `path` must be a valid, NUL-terminated C string or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_config_load(path: *const c_char) -> ConfigHandle {
    let Some(path_str) = (unsafe { cstr_to_str(path) }) else {
        return ptr::null_mut();
    };
    match Config::load(Path::new(path_str)) {
        Ok(cfg) => Box::into_raw(Box::new(cfg)) as ConfigHandle,
        Err(_) => ptr::null_mut(),
    }
}

/// Serialize a loaded config to a pretty-printed TOML string.
///
/// Returns a heap-allocated C string that the caller must free with
/// [`btrdasd_string_free`], or null on error.
///
/// # Safety
///
/// `handle` must be a valid [`ConfigHandle`] returned by [`btrdasd_config_load`]
/// or null (returns null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_config_get_toml(handle: ConfigHandle) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let config = unsafe { &*(handle as *const Config) };
    match config.to_toml() {
        Ok(toml) => string_to_c(toml),
        Err(_) => ptr::null_mut(),
    }
}

/// Validate a config and return a JSON array of error strings.
///
/// Returns `"[]"` if the config is valid. The caller must free the returned
/// string with [`btrdasd_string_free`].
///
/// # Safety
///
/// `handle` must be a valid [`ConfigHandle`] or null (returns null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_config_validate(handle: ConfigHandle) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let config = unsafe { &*(handle as *const Config) };
    let errors = config.validate();
    match serde_json::to_string(&errors) {
        Ok(json) => string_to_c(json),
        Err(_) => ptr::null_mut(),
    }
}

/// Free a [`ConfigHandle`] previously returned by [`btrdasd_config_load`].
///
/// No-op if `handle` is null. Double-free is undefined behavior.
///
/// # Safety
///
/// `handle` must be a valid [`ConfigHandle`] that has not already been freed,
/// or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_config_free(handle: ConfigHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle as *mut Config) });
    }
}

// ---------------------------------------------------------------------------
// Subvol functions
// ---------------------------------------------------------------------------

/// List all configured subvolumes as a JSON array.
///
/// Each element has the shape:
/// ```json
/// {"source":"label","name":"@home","schedule":"daily","manual":false}
/// ```
///
/// The `schedule` field is `"manual"` when `manual_only` is true, otherwise
/// `"daily"` (the default btrbk schedule).
///
/// The caller must free the returned string with [`btrdasd_string_free`].
///
/// # Safety
///
/// `handle` must be a valid [`ConfigHandle`] or null (returns null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_subvol_list(handle: ConfigHandle) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let config = unsafe { &*(handle as *const Config) };
    let infos = list_subvolumes(config);

    let json_values: Vec<serde_json::Value> = infos
        .iter()
        .map(|sv| {
            serde_json::json!({
                "source": sv.source_label,
                "name": sv.name,
                "schedule": if sv.manual_only { "manual" } else { "daily" },
                "manual": sv.manual_only,
            })
        })
        .collect();

    match serde_json::to_string(&json_values) {
        Ok(json) => string_to_c(json),
        Err(_) => ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------
// Health functions
// ---------------------------------------------------------------------------

/// Parse growth log content into a JSON array of growth points.
///
/// Each element has the shape:
/// ```json
/// {"date":"2026-01-15","label":"backup-22tb","used_bytes":1234567890}
/// ```
///
/// The `date` field is derived from the Unix timestamp in the growth log.
/// The caller must free the returned string with [`btrdasd_string_free`].
///
/// # Safety
///
/// `content` must be a valid, NUL-terminated C string or null (returns null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_health_parse_growth_log(content: *const c_char) -> *mut c_char {
    let Some(content_str) = (unsafe { cstr_to_str(content) }) else {
        return ptr::null_mut();
    };
    let points = parse_growth_log(content_str);

    let json_values: Vec<serde_json::Value> = points
        .iter()
        .map(|gp| {
            // Convert Unix timestamp to ISO date string (YYYY-MM-DD)
            let date = timestamp_to_date(gp.timestamp);
            serde_json::json!({
                "date": date,
                "label": gp.target_label,
                "used_bytes": gp.used_bytes,
            })
        })
        .collect();

    match serde_json::to_string(&json_values) {
        Ok(json) => string_to_c(json),
        Err(_) => ptr::null_mut(),
    }
}

/// Convert a Unix timestamp (seconds) to an ISO date string `YYYY-MM-DD`.
fn timestamp_to_date(timestamp: i64) -> String {
    let days = timestamp / 86400;
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
/// Uses the proleptic Gregorian calendar algorithm from civil.h (Howard Hinnant).
fn days_to_ymd(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

// ---------------------------------------------------------------------------
// Database functions
// ---------------------------------------------------------------------------

/// Open (or create) the SQLite database at `path`.
///
/// Returns a [`DbHandle`] on success, or null on error.
///
/// # Safety
///
/// `path` must be a valid, NUL-terminated C string or null (returns null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_db_open(path: *const c_char) -> DbHandle {
    let Some(path_str) = (unsafe { cstr_to_str(path) }) else {
        return ptr::null_mut();
    };
    match Database::open(path_str) {
        Ok(db) => Box::into_raw(Box::new(db)) as DbHandle,
        Err(_) => ptr::null_mut(),
    }
}

/// Get backup run history as a JSON array, ordered newest first.
///
/// Each element has the shape:
/// ```json
/// {
///   "id": 1,
///   "timestamp": 1234567890,
///   "mode": "incremental",
///   "success": true,
///   "snaps_created": 5,
///   "snaps_sent": 5,
///   "bytes_sent": 1073741824,
///   "duration_secs": 3600,
///   "errors": []
/// }
/// ```
///
/// The caller must free the returned string with [`btrdasd_string_free`].
///
/// # Safety
///
/// `handle` must be a valid [`DbHandle`] or null (returns null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_db_get_backup_history(
    handle: DbHandle,
    limit: usize,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let db = unsafe { &*(handle as *const Database) };
    let records = match db.get_backup_history(limit) {
        Ok(r) => r,
        Err(_) => return ptr::null_mut(),
    };

    let json_values: Vec<serde_json::Value> = records
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "timestamp": r.timestamp,
                "mode": r.mode,
                "success": r.success,
                "snaps_created": r.snaps_created,
                "snaps_sent": r.snaps_sent,
                "bytes_sent": r.bytes_sent,
                "duration_secs": r.duration_secs,
                "errors": r.errors,
            })
        })
        .collect();

    match serde_json::to_string(&json_values) {
        Ok(json) => string_to_c(json),
        Err(_) => ptr::null_mut(),
    }
}

/// Get target disk usage history as a JSON array for a specific target label
/// over the last `days` days.
///
/// Each element has the shape:
/// ```json
/// {
///   "id": 1,
///   "timestamp": 1234567890,
///   "target_label": "primary-22tb",
///   "total_bytes": 22000000000000,
///   "used_bytes": 5000000000000,
///   "snapshot_count": 150
/// }
/// ```
///
/// The caller must free the returned string with [`btrdasd_string_free`].
///
/// # Safety
///
/// `handle` must be a valid [`DbHandle`] or null (returns null).
/// `label` must be a valid, NUL-terminated C string or null (returns null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_db_get_target_usage(
    handle: DbHandle,
    label: *const c_char,
    days: i32,
) -> *mut c_char {
    if handle.is_null() {
        return ptr::null_mut();
    }
    let Some(label_str) = (unsafe { cstr_to_str(label) }) else {
        return ptr::null_mut();
    };
    let db = unsafe { &*(handle as *const Database) };

    // Clamp negative days to 0 (i.e., nothing matches), then convert to u32
    let days_u32 = if days < 0 { 0u32 } else { days as u32 };
    let records = match db.get_target_usage_history(label_str, days_u32) {
        Ok(r) => r,
        Err(_) => return ptr::null_mut(),
    };

    let json_values: Vec<serde_json::Value> = records
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "timestamp": r.timestamp,
                "target_label": r.target_label,
                "total_bytes": r.total_bytes,
                "used_bytes": r.used_bytes,
                "snapshot_count": r.snapshot_count,
            })
        })
        .collect();

    match serde_json::to_string(&json_values) {
        Ok(json) => string_to_c(json),
        Err(_) => ptr::null_mut(),
    }
}

/// Free a [`DbHandle`] previously returned by [`btrdasd_db_open`].
///
/// No-op if `handle` is null. Double-free is undefined behavior.
///
/// # Safety
///
/// `handle` must be a valid [`DbHandle`] that has not already been freed,
/// or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_db_free(handle: DbHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle as *mut Database) });
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Format a byte count into a human-readable string (e.g., `"1.50 GiB"`).
///
/// The caller must free the returned string with [`btrdasd_string_free`].
#[unsafe(no_mangle)]
pub extern "C" fn btrdasd_format_bytes(bytes: u64) -> *mut c_char {
    string_to_c(format_bytes(bytes))
}

/// Free a C string previously returned by any `btrdasd_*` function.
///
/// No-op if `s` is null. Double-free is undefined behavior.
///
/// # Safety
///
/// `s` must be a pointer returned by a `btrdasd_*` function that has not
/// already been freed, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn btrdasd_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn format_bytes_roundtrip() {
        let result = btrdasd_format_bytes(1_073_741_824);
        assert!(!result.is_null());
        let s = unsafe { CStr::from_ptr(result) }.to_str().unwrap();
        assert_eq!(s, "1.00 GiB");
        unsafe { btrdasd_string_free(result) };
    }

    #[test]
    fn format_bytes_zero() {
        let result = btrdasd_format_bytes(0);
        assert!(!result.is_null());
        let s = unsafe { CStr::from_ptr(result) }.to_str().unwrap();
        assert_eq!(s, "0 B");
        unsafe { btrdasd_string_free(result) };
    }

    #[test]
    fn string_free_null_is_noop() {
        // Must not crash
        unsafe { btrdasd_string_free(ptr::null_mut()) };
    }

    #[test]
    fn config_load_null_path_returns_null() {
        let handle = unsafe { btrdasd_config_load(ptr::null()) };
        assert!(handle.is_null());
    }

    #[test]
    fn config_load_nonexistent_returns_null() {
        let path = CString::new("/nonexistent/path/to/config.toml").unwrap();
        let handle = unsafe { btrdasd_config_load(path.as_ptr()) };
        assert!(handle.is_null());
    }

    #[test]
    fn config_get_toml_null_handle_returns_null() {
        let result = unsafe { btrdasd_config_get_toml(ptr::null_mut()) };
        assert!(result.is_null());
    }

    #[test]
    fn config_validate_null_handle_returns_null() {
        let result = unsafe { btrdasd_config_validate(ptr::null_mut()) };
        assert!(result.is_null());
    }

    #[test]
    fn config_free_null_is_noop() {
        unsafe { btrdasd_config_free(ptr::null_mut()) };
    }

    #[test]
    fn config_load_validate_free_cycle() {
        // Create a minimal valid config as a temp file
        let toml = r#"
[general]
version = "0.5.1"
install_prefix = "/usr"
db_path = "/tmp/test.db"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[[source]]
label = "test"
volume = "/vol"
device = "/dev/sda"
subvolumes = ["@"]
[[target]]
label = "t"
serial = "X"
mount = "/mnt/t"
role = "primary"
[target.retention]
weekly = 4
[esp]
enabled = false
[email]
enabled = false
[gui]
enabled = false
"#;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), toml).unwrap();
        let path_str = tmp.path().to_str().unwrap();
        let c_path = CString::new(path_str).unwrap();

        // Load
        let handle = unsafe { btrdasd_config_load(c_path.as_ptr()) };
        assert!(!handle.is_null(), "config should load successfully");

        // Get TOML
        let toml_out = unsafe { btrdasd_config_get_toml(handle) };
        assert!(!toml_out.is_null());
        let toml_str = unsafe { CStr::from_ptr(toml_out) }.to_str().unwrap();
        assert!(toml_str.contains("version"));
        unsafe { btrdasd_string_free(toml_out) };

        // Validate
        let errors = unsafe { btrdasd_config_validate(handle) };
        assert!(!errors.is_null());
        let errors_str = unsafe { CStr::from_ptr(errors) }.to_str().unwrap();
        let parsed: Vec<String> = serde_json::from_str(errors_str).unwrap();
        assert!(parsed.is_empty(), "valid config should have no errors");
        unsafe { btrdasd_string_free(errors) };

        // Subvol list
        let subvols = unsafe { btrdasd_subvol_list(handle) };
        assert!(!subvols.is_null());
        let subvols_str = unsafe { CStr::from_ptr(subvols) }.to_str().unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(subvols_str).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["name"], "@");
        assert_eq!(parsed[0]["source"], "test");
        assert_eq!(parsed[0]["manual"], false);
        unsafe { btrdasd_string_free(subvols) };

        // Free
        unsafe { btrdasd_config_free(handle) };
    }

    #[test]
    fn subvol_list_null_handle_returns_null() {
        let result = unsafe { btrdasd_subvol_list(ptr::null_mut()) };
        assert!(result.is_null());
    }

    #[test]
    fn health_parse_growth_log_valid() {
        let log = CString::new(
            "1709000000 primary-22tb 5368709120\n1709086400 primary-22tb 5905580032\n",
        )
        .unwrap();
        let result = unsafe { btrdasd_health_parse_growth_log(log.as_ptr()) };
        assert!(!result.is_null());
        let s = unsafe { CStr::from_ptr(result) }.to_str().unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(s).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["label"], "primary-22tb");
        assert_eq!(parsed[0]["used_bytes"], 5368709120u64);
        // Verify date is a valid ISO date string
        let date = parsed[0]["date"].as_str().unwrap();
        assert!(date.len() == 10 && date.contains('-'));
        unsafe { btrdasd_string_free(result) };
    }

    #[test]
    fn health_parse_growth_log_null_returns_null() {
        let result = unsafe { btrdasd_health_parse_growth_log(ptr::null()) };
        assert!(result.is_null());
    }

    #[test]
    fn health_parse_growth_log_empty() {
        let log = CString::new("").unwrap();
        let result = unsafe { btrdasd_health_parse_growth_log(log.as_ptr()) };
        assert!(!result.is_null());
        let s = unsafe { CStr::from_ptr(result) }.to_str().unwrap();
        assert_eq!(s, "[]");
        unsafe { btrdasd_string_free(result) };
    }

    #[test]
    fn db_open_null_returns_null() {
        let result = unsafe { btrdasd_db_open(ptr::null()) };
        assert!(result.is_null());
    }

    #[test]
    fn db_open_and_free_cycle() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path_str = tmp.path().to_str().unwrap();
        let c_path = CString::new(path_str).unwrap();

        let handle = unsafe { btrdasd_db_open(c_path.as_ptr()) };
        assert!(!handle.is_null(), "database should open successfully");

        // Get empty backup history
        let history = unsafe { btrdasd_db_get_backup_history(handle, 10) };
        assert!(!history.is_null());
        let s = unsafe { CStr::from_ptr(history) }.to_str().unwrap();
        assert_eq!(s, "[]");
        unsafe { btrdasd_string_free(history) };

        // Get empty target usage
        let label = CString::new("test").unwrap();
        let usage = unsafe { btrdasd_db_get_target_usage(handle, label.as_ptr(), 30) };
        assert!(!usage.is_null());
        let s = unsafe { CStr::from_ptr(usage) }.to_str().unwrap();
        assert_eq!(s, "[]");
        unsafe { btrdasd_string_free(usage) };

        unsafe { btrdasd_db_free(handle) };
    }

    #[test]
    fn db_get_backup_history_null_handle_returns_null() {
        let result = unsafe { btrdasd_db_get_backup_history(ptr::null_mut(), 10) };
        assert!(result.is_null());
    }

    #[test]
    fn db_get_target_usage_null_handle_returns_null() {
        let label = CString::new("test").unwrap();
        let result = unsafe { btrdasd_db_get_target_usage(ptr::null_mut(), label.as_ptr(), 30) };
        assert!(result.is_null());
    }

    #[test]
    fn db_get_target_usage_null_label_returns_null() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path_str = tmp.path().to_str().unwrap();
        let c_path = CString::new(path_str).unwrap();
        let handle = unsafe { btrdasd_db_open(c_path.as_ptr()) };
        assert!(!handle.is_null());

        let result = unsafe { btrdasd_db_get_target_usage(handle, ptr::null(), 30) };
        assert!(result.is_null());

        unsafe { btrdasd_db_free(handle) };
    }

    #[test]
    fn db_free_null_is_noop() {
        unsafe { btrdasd_db_free(ptr::null_mut()) };
    }

    #[test]
    fn timestamp_to_date_epoch() {
        assert_eq!(timestamp_to_date(0), "1970-01-01");
    }

    #[test]
    fn timestamp_to_date_known() {
        // 2024-02-29 00:00:00 UTC = 1709164800
        assert_eq!(timestamp_to_date(1709164800), "2024-02-29");
    }
}
