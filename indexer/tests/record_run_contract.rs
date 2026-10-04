//! The contract between `scripts/backup-run.sh` and `btrdasd backup record-run`
//! (bd DAS-Backup-Manager-6wt).
//!
//! A failed backup run left no `backup_runs` row. When `btrbk list latest`
//! failed, the script passed `--snaps-created -1 --snaps-sent -1`; clap read
//! `-1` as an option it did not know and refused the whole call; the script
//! logged the refusal as non-fatal. The history then showed the previous
//! successful run as the latest one. Each side's own tests passed throughout:
//! the script's tests stub `btrdasd` with something that accepts anything, and
//! the binary's tests never see the script's arguments.
//!
//! So this test builds the argument vector the way the script does —
//! `record_run_args`, extracted from the live script by name, not a copy of it —
//! hands it to the real binary against a temporary database, and reads the row
//! back with plain SQL rather than through the reader under test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Runs `record_run_args` from the script after `$setup` and prints the vector
/// NUL-separated, so no argument can be split or joined on the way out.
/// `BTRBK_LATEST_RAW_OK` starts at the script's own initial value — the value an
/// abort before `capture_report_data` leaves behind.
const HARNESS: &str = r#"
set -euo pipefail
script="$1" setup="$2" status="$3" full="$4"
extract() { sed -n "/^$1() {/,/^}/p" "$script"; }
for fn in decide_run_counts record_run_args record_op note_abort; do
    body="$(extract "$fn")"
    if [[ -z "$body" ]]; then
        echo "HARNESS BROKEN: $fn() not found in $script" >&2
        exit 3
    fi
    eval "$body"
done
log_warn() { :; }
declare -A OP_STATUS=() USAGE_BEFORE=() USAGE_AFTER=()
ALL_TARGET_MOUNTS=()
BTRBK_START_TIME=0
BTRBK_END_TIME=0
BTRBK_LATEST_RAW=""
init="$(grep -m1 '^BTRBK_LATEST_RAW_OK=' "$script")" || true
if [[ -z "$init" ]]; then
    echo "HARNESS BROKEN: no BTRBK_LATEST_RAW_OK= line in $script" >&2
    exit 3
fi
eval "$init"
eval "$setup"
record_run_args "$status" "$full"
printf '%s\0' "${RECORD_RUN_ARGS[@]}"
"#;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/backup-run.sh")
}

/// The vector the script builds for a run with this state.
fn script_vector(setup: &str, status: &str, full: &str) -> Vec<String> {
    let out = Command::new("bash")
        .arg("-c")
        .arg(HARNESS)
        .arg("record-run-contract")
        .arg(script())
        .arg(setup)
        .arg(status)
        .arg(full)
        .output()
        .expect("spawn bash");
    assert!(
        out.status.success(),
        "harness exited {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("vector is UTF-8")
        .split_terminator('\0')
        .map(str::to_owned)
        .collect()
}

fn btrdasd(args: &[String]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_btrdasd"))
        .args(args)
        .output()
        .expect("spawn btrdasd")
}

#[derive(Debug, PartialEq)]
struct Row {
    id: i64,
    success: i64,
    mode: String,
    snaps_created: Option<i64>,
    snaps_sent: Option<i64>,
    bytes_sent: i64,
    duration_secs: i64,
    errors: String,
}

/// Every row, read with plain SQL — not through the reader under test.
fn rows(db: &Path) -> Vec<Row> {
    let conn = rusqlite::Connection::open(db).expect("open the test database");
    let mut stmt = conn
        .prepare(
            "SELECT id, success, mode, snaps_created, snaps_sent, bytes_sent, duration_secs, errors
             FROM backup_runs ORDER BY id",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok(Row {
            id: r.get(0)?,
            success: r.get(1)?,
            mode: r.get(2)?,
            snaps_created: r.get(3)?,
            snaps_sent: r.get(4)?,
            bytes_sent: r.get(5)?,
            duration_secs: r.get(6)?,
            errors: r.get(7)?,
        })
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// Records the run the script would record, and returns the vector it used.
fn record_as_the_script_does(db: &Path, state: &str, status: &str, full: &str) -> Vec<String> {
    let setup = format!("DAS_DB_PATH='{}'\n{state}", db.display());
    let vector = script_vector(&setup, status, full);
    let out = btrdasd(&vector);
    assert!(
        out.status.success(),
        "btrdasd refused the script's vector {vector:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    vector
}

/// Two source subvolumes, each sent to two targets, plus one row whose target
/// is not present (`target_subvolume=''`) — `btrbk --format=raw list latest` as
/// btrbk 0.32.7 prints it (`print_formatted`, `latest` → `raw`).
const RAW_LISTING: &str = "\
format=\"latest\" type='snapshot' source_url='/.btrfs-nvme/@' source_host='' source_port='' source_subvolume='/.btrfs-nvme/@' snapshot_subvolume='/.btrfs-nvme/.btrbk-snapshots/root-.20261003T0300' snapshot_name='root-' status='' target_url='/mnt/backup-22tb/nvme/root-.20261003T0300' target_host='' target_port='' target_subvolume='/mnt/backup-22tb/nvme/root-.20261003T0300' target_type='send-receive' source_rsh='' target_rsh=''
format=\"latest\" type='snapshot' source_url='/.btrfs-nvme/@' source_host='' source_port='' source_subvolume='/.btrfs-nvme/@' snapshot_subvolume='/.btrfs-nvme/.btrbk-snapshots/root-.20261003T0300' snapshot_name='root-' status='' target_url='/mnt/backup-system-recovery-B/nvme/root-.20261003T0300' target_host='' target_port='' target_subvolume='/mnt/backup-system-recovery-B/nvme/root-.20261003T0300' target_type='send-receive' source_rsh='' target_rsh=''
format=\"latest\" type='snapshot' source_url='/.btrfs-nvme/@home' source_host='' source_port='' source_subvolume='/.btrfs-nvme/@home' snapshot_subvolume='/.btrfs-nvme/.btrbk-snapshots/home.20261003T0300' snapshot_name='home' status='' target_url='/mnt/backup-22tb/nvme/home.20261003T0300' target_host='' target_port='' target_subvolume='/mnt/backup-22tb/nvme/home.20261003T0300' target_type='send-receive' source_rsh='' target_rsh=''
format=\"latest\" type='snapshot' source_url='/.btrfs-nvme/@home' source_host='' source_port='' source_subvolume='/.btrfs-nvme/@home' snapshot_subvolume='/.btrfs-nvme/.btrbk-snapshots/home.20261003T0300' snapshot_name='home' status='' target_url='/mnt/backup-system-recovery-B/nvme/home.20261003T0300' target_host='' target_port='' target_subvolume='/mnt/backup-system-recovery-B/nvme/home.20261003T0300' target_type='send-receive' source_rsh='' target_rsh=''
format=\"latest\" type='snapshot' source_url='/.btrfs-nvme/@home' source_host='' source_port='' source_subvolume='/.btrfs-nvme/@home' snapshot_subvolume='/.btrfs-nvme/.btrbk-snapshots/home.20261003T0300' snapshot_name='home' status='' target_url='' target_host='' target_port='' target_subvolume='' target_type='' source_rsh='' target_rsh=''
";

/// Script state for a run that went through `capture_report_data` and read
/// `RAW_LISTING`: two snapshots, four sends, 3096 bytes, 516 s.
fn normal_state() -> String {
    format!(
        "ALL_TARGET_MOUNTS=(/mnt/backup-22tb /mnt/backup-system-recovery-B)
USAGE_BEFORE=([/mnt/backup-22tb]=1000 [/mnt/backup-system-recovery-B]=500)
USAGE_AFTER=([/mnt/backup-22tb]=4096 [/mnt/backup-system-recovery-B]=500)
BTRBK_START_TIME=1000
BTRBK_END_TIME=1516
BTRBK_LATEST_RAW_OK=true
BTRBK_LATEST_RAW=\"$(cat <<'RAW'
{RAW_LISTING}RAW
)\""
    )
}

/// The live 2026-10-02 failure: recovery drive A absent, btrbk exit 10,
/// `btrbk list latest` failed, so the counters could not be read.
const UNKNOWN_COUNT_STATE: &str = "ALL_TARGET_MOUNTS=(/mnt/backup-22tb)
USAGE_BEFORE=([/mnt/backup-22tb]=1000)
USAGE_AFTER=([/mnt/backup-22tb]=1000)
BTRBK_START_TIME=1000
BTRBK_END_TIME=1516
OP_STATUS[btrbk]=FAIL
OP_STATUS[btrbk_detail]='exit code 10'
BTRBK_LATEST_RAW_OK=false";

#[test]
fn a_normal_run_is_recorded_with_its_counts() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");

    let vector = record_as_the_script_does(&db, &normal_state(), "SUCCESS", "false");

    assert!(
        !vector.iter().any(|a| a == "--counts-unknown"),
        "{vector:?}"
    );
    assert_eq!(
        rows(&db),
        vec![Row {
            id: 1,
            success: 1,
            mode: "incremental".into(),
            snaps_created: Some(2),
            snaps_sent: Some(4),
            bytes_sent: 3096,
            duration_secs: 516,
            errors: String::new(),
        }]
    );
}

#[test]
fn a_run_whose_counts_are_unknown_is_recorded_with_null_counts() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");

    let vector = record_as_the_script_does(&db, UNKNOWN_COUNT_STATE, "FAILURE", "true");

    assert!(
        !vector.iter().any(|a| a.starts_with("--snaps-")),
        "an unknown count must not travel as a number: {vector:?}"
    );
    let got = rows(&db);
    assert_eq!(
        got.len(),
        1,
        "the failed run must be in the history: {got:?}"
    );
    let row = &got[0];
    assert_eq!(row.success, 0);
    assert_eq!(row.mode, "full");
    assert_eq!((row.snaps_created, row.snaps_sent), (None, None), "{row:?}");
    assert_eq!((row.bytes_sent, row.duration_secs), (0, 516));
    let mut errors: Vec<&str> = row.errors.lines().collect();
    errors.sort_unstable();
    assert_eq!(
        errors,
        vec![
            "btrbk: exit code 10",
            "btrbk_counters: btrbk list latest failed; counts unknown"
        ]
    );
}

#[test]
fn a_run_that_ended_before_the_counters_were_read_is_recorded_with_null_counts() {
    // The abort path: cleanup() records the run, but capture_report_data never
    // ran, so BTRBK_LATEST_RAW_OK still holds the script's initial value. Before
    // 6wt that value was "true" and the row read 0 created, 0 sent.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    let state = "ALL_TARGET_MOUNTS=(/mnt/backup-22tb)
BTRBK_START_TIME=1000
BTRBK_END_TIME=1060";

    record_as_the_script_does(&db, state, "FAILURE", "false");

    let got = rows(&db);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].snaps_created, got[0].snaps_sent), (None, None));
    assert_eq!(
        got[0].errors,
        "btrbk_counters: not read — the run ended before the snapshot counters were taken"
    );
}

/// The errors of the one row, sorted: `--errors` lists the failed operations in
/// the order of a bash associative array, which is not an order.
fn sorted_errors(db: &Path) -> Vec<String> {
    let got = rows(db);
    assert_eq!(got.len(), 1, "{got:?}");
    let mut errors: Vec<String> = got[0].errors.lines().map(str::to_owned).collect();
    errors.sort_unstable();
    errors
}

#[test]
fn an_aborted_run_is_recorded_as_failed_with_what_stopped_it() {
    // bd DAS-Backup-Manager-2my: cleanup() records a run that aborted before
    // its report. abort_reason() left what stopped it in ABORT_WHAT/ABORT_REASON
    // (two violations here, so two lines), note_abort() turned that into an
    // `aborted` FAIL, and capture_report_data never ran: the counts are unknown
    // and btrbk ran for 0 s.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    let state = r#"ALL_TARGET_MOUNTS=(/mnt/backup-22tb /mnt/backup-system-recovery-A)
ABORT_WHAT='target verification'
ABORT_REASON="$(printf '%s\n%s' "primary-22tb: /mnt/backup-22tb has fs UUID 'x', expected 'y' — different filesystem mounted here" "system-recovery-A-2tb: marked unavailable but /mnt/backup-system-recovery-A still exists (would let btrbk write to bare dir on /)")"
note_abort 3 'exit 3'"#;

    let vector = record_as_the_script_does(&db, state, "FAILURE", "true");

    assert!(!vector.iter().any(|a| a == "--success"), "{vector:?}");
    assert!(vector.iter().any(|a| a == "--counts-unknown"), "{vector:?}");
    let got = rows(&db);
    assert_eq!(
        got.len(),
        1,
        "the aborted run must be in the history: {got:?}"
    );
    let row = &got[0];
    assert_eq!(row.success, 0);
    assert_eq!(row.mode, "full");
    assert_eq!((row.snaps_created, row.snaps_sent), (None, None), "{row:?}");
    assert_eq!(
        (row.bytes_sent, row.duration_secs),
        (0, 0),
        "btrbk never ran"
    );
    assert_eq!(
        sorted_errors(&db),
        vec![
            "aborted: target verification: primary-22tb: /mnt/backup-22tb has fs UUID 'x', \
             expected 'y' — different filesystem mounted here; system-recovery-A-2tb: marked \
             unavailable but /mnt/backup-system-recovery-A still exists (would let btrbk write \
             to bare dir on /)",
            "btrbk_counters: not read — the run ended before the snapshot counters were taken",
        ]
    );
}

#[test]
fn an_implicit_abort_is_recorded_with_the_command_that_failed() {
    // A command failing under set -e, with no abort_reason() before it: cleanup()
    // hands note_abort() the status and $BASH_COMMAND. Quotes and `$` in the
    // command travel as one argument, untouched.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    let state = r#"ALL_TARGET_MOUNTS=(/mnt/backup-22tb)
note_abort 32 'mount -t btrfs -o subvolid=5 "$dev" "$mnt"'"#;

    record_as_the_script_does(&db, state, "FAILURE", "false");

    assert_eq!(
        sorted_errors(&db),
        vec![
            r#"aborted: a command that failed: exit status 32: mount -t btrfs -o subvolid=5 "$dev" "$mnt""#,
            "btrbk_counters: not read — the run ended before the snapshot counters were taken",
        ]
    );
}

#[test]
fn the_history_shows_an_unknown_count_as_unknown_and_json_as_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    record_as_the_script_does(&db, &normal_state(), "SUCCESS", "false");
    record_as_the_script_does(&db, UNKNOWN_COUNT_STATE, "FAILURE", "false");
    let db_arg = db.to_str().unwrap().to_owned();

    let text = btrdasd(&[
        "backup".into(),
        "report".into(),
        "--db".into(),
        db_arg.clone(),
    ]);
    assert!(text.status.success(), "{text:?}");
    let text = String::from_utf8(text.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    // Same timestamp second for both runs, so the order between them is not
    // what is under test: find each by its status column.
    let failed = lines.iter().find(|l| l.contains(" FAIL ")).expect(&text);
    let ok = lines.iter().find(|l| l.contains(" OK ")).expect(&text);
    // The last two columns, Created and Sent.
    fn counts(line: &str) -> Vec<&str> {
        let words: Vec<&str> = line.split_whitespace().collect();
        words[words.len() - 2..].to_vec()
    }
    assert_eq!(counts(failed), vec!["unknown", "unknown"], "{text}");
    assert_eq!(counts(ok), vec!["2", "4"], "{text}");

    let json = btrdasd(&[
        "--json".into(),
        "backup".into(),
        "report".into(),
        "--db".into(),
        db_arg,
    ]);
    assert!(json.status.success(), "{json:?}");
    let runs: serde_json::Value = serde_json::from_slice(&json.stdout).expect("valid JSON");
    let runs = runs.as_array().expect("an array");
    assert_eq!(runs.len(), 2, "{runs:?}");
    let by_success = |s: bool| {
        runs.iter()
            .find(|r| r["success"] == serde_json::Value::Bool(s))
            .expect("run present")
    };
    assert_eq!(by_success(false)["snaps_created"], serde_json::Value::Null);
    assert_eq!(by_success(false)["snaps_sent"], serde_json::Value::Null);
    assert_eq!(by_success(true)["snaps_created"], 2);
    assert_eq!(by_success(true)["snaps_sent"], 4);
}
