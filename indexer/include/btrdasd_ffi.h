/*
 * btrdasd_ffi.h — C-ABI interface to the buttered_dasd Rust library.
 *
 * Link with: -lbuttered_dasd_ffi
 *
 * Memory contract:
 *   - Functions returning char* allocate with Rust's allocator.
 *   - The caller MUST free those strings with btrdasd_string_free().
 *   - Do NOT pass Rust-allocated strings to free() or delete[].
 *   - Opaque handles must be freed with their corresponding _free function.
 *   - All functions tolerate NULL handles/pointers (return NULL or no-op).
 */

#ifndef BTRDASD_FFI_H
#define BTRDASD_FFI_H

#include <stddef.h>  /* size_t */
#include <stdint.h>  /* uint64_t, int32_t */

#ifdef __cplusplus
extern "C" {
#endif

/* ========================================================================= */
/* Opaque handle types                                                       */
/* ========================================================================= */

/** Opaque handle to a loaded backup configuration. */
typedef void* BtrdasdConfig;

/** Opaque handle to an open SQLite backup database. */
typedef void* BtrdasdDb;

/* ========================================================================= */
/* Configuration                                                             */
/* ========================================================================= */

/**
 * Load a TOML configuration file.
 *
 * @param path  Null-terminated path to config.toml.
 * @return      Config handle, or NULL on error.
 */
BtrdasdConfig btrdasd_config_load(const char* path);

/**
 * Serialize the config back to a TOML string.
 *
 * @param handle  Config handle (from btrdasd_config_load).
 * @return        TOML string (caller must free with btrdasd_string_free),
 *                or NULL on error.
 */
char* btrdasd_config_get_toml(BtrdasdConfig handle);

/**
 * Validate the configuration.
 *
 * @param handle  Config handle.
 * @return        JSON array of error strings, e.g. '["no sources"]'.
 *                Returns '[]' if valid.  Caller must free with btrdasd_string_free.
 *                Returns NULL if handle is NULL.
 */
char* btrdasd_config_validate(BtrdasdConfig handle);

/**
 * Free a config handle.  No-op if handle is NULL.
 */
void btrdasd_config_free(BtrdasdConfig handle);

/* ========================================================================= */
/* Subvolume management                                                      */
/* ========================================================================= */

/**
 * List all configured subvolumes across all sources.
 *
 * @param handle  Config handle.
 * @return        JSON array:
 *                [{"source":"label","name":"@home","schedule":"daily","manual":false}, ...]
 *                Caller must free with btrdasd_string_free.
 *                Returns NULL if handle is NULL.
 */
char* btrdasd_subvol_list(BtrdasdConfig handle);

/* ========================================================================= */
/* Health / growth log                                                       */
/* ========================================================================= */

/**
 * Parse a growth log string into structured data.
 *
 * @param content  Null-terminated growth log text.
 * @return         JSON array:
 *                 [{"date":"2026-01-15","label":"backup-22tb","used_bytes":1234567890}, ...]
 *                 Caller must free with btrdasd_string_free.
 *                 Returns NULL if content is NULL.
 */
char* btrdasd_health_parse_growth_log(const char* content);

/* ========================================================================= */
/* Database                                                                  */
/* ========================================================================= */

/**
 * Open (or create) the SQLite backup index database.
 *
 * @param path  Null-terminated path to the .db file.
 * @return      Database handle, or NULL on error.
 */
BtrdasdDb btrdasd_db_open(const char* path);

/**
 * Get the most recent backup runs.
 *
 * @param handle  Database handle.
 * @param limit   Maximum number of records to return.
 * @return        JSON array of backup run records:
 *                [{"id":1,"timestamp":1234567890,"mode":"incremental",
 *                  "success":true,"snaps_created":5,"snaps_sent":5,
 *                  "bytes_sent":1073741824,"duration_secs":3600,
 *                  "errors":[]}, ...]
 *                Caller must free with btrdasd_string_free.
 *                Returns NULL on error or NULL handle.
 */
char* btrdasd_db_get_backup_history(BtrdasdDb handle, size_t limit);

/**
 * Get target disk usage history over a number of days.
 *
 * @param handle  Database handle.
 * @param label   Null-terminated target label (e.g. "backup-22tb").
 * @param days    Number of days of history to retrieve.
 * @return        JSON array of usage records:
 *                [{"id":1,"timestamp":1234567890,"label":"backup-22tb",
 *                  "total_bytes":22000000000000,"used_bytes":15000000000000,
 *                  "snapshot_count":365}, ...]
 *                Caller must free with btrdasd_string_free.
 *                Returns NULL on error or NULL handle/label.
 */
char* btrdasd_db_get_target_usage(BtrdasdDb handle, const char* label, int32_t days);

/**
 * Free a database handle.  No-op if handle is NULL.
 */
void btrdasd_db_free(BtrdasdDb handle);

/* ========================================================================= */
/* Utility                                                                   */
/* ========================================================================= */

/**
 * Format a byte count into a human-readable string (e.g. "1.50 GiB").
 *
 * @param bytes  Byte count.
 * @return       Formatted string.  Caller must free with btrdasd_string_free.
 */
char* btrdasd_format_bytes(uint64_t bytes);

/**
 * Free a string returned by any btrdasd_* function.
 * No-op if s is NULL.
 *
 * IMPORTANT: Only use this for strings returned by btrdasd_* functions.
 * Do NOT use free() or delete[] on those strings.
 */
void btrdasd_string_free(char* s);

#ifdef __cplusplus
}
#endif

#endif /* BTRDASD_FFI_H */
