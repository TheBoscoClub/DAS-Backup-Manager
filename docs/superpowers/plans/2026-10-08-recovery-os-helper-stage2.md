# Recovery-OS Helper Methods and Schedules (8249 stage 2) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `btrdasd-helper` can tell a GUI everything about each recovery drive's OS and its VM
sessions, start a session (attended or unattended, one drive or both), hand the operator a console
socket, finish a kept session, and put a per-drive or both-drives update on a date and time — all
on top of `scripts/recovery-os-vm.sh` 2.x as merged in stage 1 (commit `21aef94`).

**Architecture:** The script stays the one driver; the helper spawns it and parses its machine
lines (`PROGRESS`, `OUTPUT`, `RESULT`, `DRIVE`) into the existing job signals. Every rule the GUI
needs (can this drive run unattended, how many clean runs, which pair mode to default to, when is a
schedule due) is a pure function in a new library module `recovery_os::panel`, tested without a
bus. Scheduled sessions are generated systemd unit pairs the helper writes under
`/etc/systemd/system`, running the script unattended with a new `--wait-lock` option; they are
never transient units (a reboot before the time must not lose them silently). Five new D-Bus
methods, one new polkit action.

**Tech Stack:** Rust 2024 (`buttered_dasd`, `btrdasd-helper` on zbus 5 / tokio), bash
(`scripts/recovery-os-vm.sh`, `tests/test_recovery_os_vm.sh` stub suite), CMake/ctest, systemd.

**Spec:** the decisions on bd `DAS-Backup-Manager-8249` (notes 1–9: 2026-10-04 10:40, 11:25,
11:33, 11:39; 2026-10-07 decisions 5–9) plus the script's own header (`scripts/recovery-os-vm.sh`
lines 1–330: guarantees, usage, the progress grammar, exit statuses, test seams). Read all of it
with `bd show 8249` and `sed -n 1,330p scripts/recovery-os-vm.sh` before Task 1. The stage-3 GUI
panel is NOT in this plan; this plan's deliverable is the helper surface it will call.

## Global Constraints

- Worktree, never the main checkout: `git worktree add /tmp/das-8249-s2 -b 8249-stage2 main`. Build only through CMake into tmpfs: `cmake -S . -B /tmp/das-8249-s2-build -DCMAKE_BUILD_TYPE=Release && cmake --build /tmp/das-8249-s2-build`. Never a bare `cargo build`.
- Rust tests: `cd indexer && CARGO_TARGET_DIR=/tmp/das-8249-s2-target cargo test` and again with `--features dbus`. Shell suite: `bash tests/test_recovery_os_vm.sh` (it takes ~5 min; `REQUIRE_ALL_SHELL_CASES` is set by ctest).
- Before every commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` (both feature sets), `shellcheck scripts/*.sh tests/*.sh`, `shfmt -d -i 4 -ci scripts/recovery-os-vm.sh`, `codespell docs CHANGELOG.md`.
- Never install (`cmake --install`, `setup --upgrade`) in this plan. Install is stage 3's, and only when no `das-*` unit runs and `/run/das-backup.lock` is free.
- Bash in `scripts/`: never `local x=$(cmd)`; never pipe a listing into an early-exit reader; no `[0-9]` bracket ranges (use `[[:digit:]]`); `LC_ALL=C` is already exported by the script.
- Fail-silent law (`.claude/rules/fail-silent.md`): a missing reading is `null`/`unknown`, never `0` or `false`; a refused input is never defaulted; an accepted-and-ignored parameter is a defect; a failure branch never records success.
- The helper runs the script with an environment it builds itself: `Command::env_clear()`, then exactly `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin`, `LC_ALL=C`, `HOME=/root`. **Every `DAS_RECOVERY_*`, `DAS_CONFIG`, `BTRDASD_BIN` and `DAS_RECOVERY_OS_STATE` variable is absent** (the stage-1 review note): the test seams must be unreachable from the bus. Pinned by a test in Task 4.
- The script is run by its installed absolute path `/usr/lib/das-backup/recovery-os-vm.sh` (CMake `DAS_SCRIPT_DIR`), never found on `PATH`. The library takes the path as a parameter so tests point it at a stub.
- Polkit: new action `org.dasbackup.recovery-os` (`auth_admin_keep`, like `org.dasbackup.backup`) for session start, session-end, console and schedule writes; status reads use the existing `org.dasbackup.health` (`allow_active` yes).
- Script version `2.0.0` → `2.1.0` (header `# Version:` and `# Date:`). No project version bump (the work lands under `## [Unreleased]`; `/git-release` bumps).
- Mutation gate on the diff (copy of the tree, `indexer/`): `git diff --relative origin/main -- . > mutants.diff && env -u CARGO_TARGET_DIR cargo mutants --in-place --no-shuffle --in-diff mutants.diff && env -u CARGO_TARGET_DIR python3 ../.github/scripts/mutants-gate.py` must print `mutants OK`. Shim mail first (a `mailx`/`s-nail`/`sendmail` stub first on PATH) — a mutation run has mailed through the live relay before (bd 1x76).
- Commits: signed, heredoc message, no trailer, no AI attribution of any kind. `~/.claude/bin/scrub-promo check origin/main..HEAD` must print CLEAN before any push. `.staged-release` exists: commits may be pushed, **no tag, no `gh release`**.
- Every new interactive element in later GUI work needs a tooltip; not this plan's concern, but every JSON field this plan produces must carry enough words for one (reasons, never bare booleans).

## Review Focus

1. **A schedule whose time has passed while the host was off.** Expected: the status JSON says `missed` with the time, and nothing runs at the next boot (`Persistent=false` in the timer). Pinned in Task 6 (`a_timer_whose_time_passed_without_a_trigger_reads_missed`).
2. **A scheduled session refused at its start (lock held by a backup that overran 3 hours, or the record says `will`).** Expected: the history has no line (the script writes one only after it took the lock), so status must read the unit's result and last journal lines and say `refused: …`, never `never ran` or `clean`. Pinned in Task 6 (`a_fired_schedule_with_no_history_line_reads_the_units_result`).
3. **Two session jobs at once** (two GUI windows, or a schedule firing during a Now session). Expected: the second is refused before the script starts, naming the first (job id or unit); the script's lock would refuse it anyway, but then only as exit 1 after a `PROGRESS` line. Pinned in Task 4 (`a_second_session_job_is_refused_naming_the_first`).
4. **A `RESULT` line for a label the request did not name, or a `PROGRESS` line with an unknown step.** Expected: passed through as a log line, never dropped and never mapped to a stage; the final outcome comes from the exit status, not from the lines. Pinned in Task 3 (`unknown_lines_are_logged_not_lost`).
5. **`RecoveryOsConsole` for a drive whose session is not running, or from a caller whose uid the bus cannot name.** Expected: an error with the script's refusal text, no socket left behind; a uid lookup that fails is an error, never uid 0. Pinned in Task 5.

## Decisions taken in this plan (for the operator to confirm at review)

- **Schedules are persistent generated units**, not `systemd-run` transient timers: `das-recovery-os-update-<label>.{timer,service}` for one drive, `das-recovery-os-update-both.{timer,service}` for both. `Persistent=false`: a time that passed while the host was off is reported `missed`, never run at the next boot behind the operator's back.
- **A scheduled session waits for the lock** (`--wait-lock 180`, new script option) instead of refusing: at the chosen time a backup may be running. The "Now" path keeps the immediate refusal.
- **Scheduled sessions are always unattended** (the spec's own reading: an attended one would hold the maintenance lock at an idle console). The helper refuses to schedule a drive whose record does not admit unattended, with the reason.
- **A session job's `JobFinished(success)` is `true` only for exit 0.** Exit 5 (warnings) is `false` with a summary beginning `warnings:`; the GUI (stage 3) distinguishes by the summary's first word. Reason: `JobFinished` has one boolean and the operator asked that a warned session "needs a look".
- **`JobCancel` on a session job sends the script SIGINT once** and the job ends with the script's own exit (3: VM left running, lock kept, `session-end` finishes). Cancel never destroys a running recovery OS.
- **The generated service units carry `SuccessExitStatus=1 3 4 5 6 7`**: the script's outcomes travel by the history, the summary and the journal, never by a `failed` unit that `cachyos-sentinel` would restart (a restarted VM session is exactly what must never happen unasked). `backup.md` §Sentinel Interaction gets a row.
- **`clean-runs` and `history` are read by running the script**, never by a second JSON reader in Rust (single canonical source: the script's `read_history` checks each line).

## File Structure

| File | Responsibility |
|---|---|
| `scripts/recovery-os-vm.sh` (modify) | `--wait-lock <minutes>` for `session` (single and pair); version 2.1.0 |
| `tests/test_recovery_os_vm.sh` (modify) | the wait-lock cases |
| `indexer/src/recovery_os/panel.rs` (create) | pure rules: `unattended_possible`, `pair_mode_default`, `due`, the status JSON builder, the schedule unit renderer and reader, the session line parser |
| `indexer/src/recovery_os/session.rs` (create) | spawning the script: the `SessionSpawner` seam, the job driver that turns lines into `ProgressSink` events, cancel by SIGINT |
| `indexer/src/recovery_os.rs` (modify) | `pub mod panel; pub mod session;` and `GuestAgent::runs_at_boot` reuse |
| `indexer/src/bin/btrdasd-helper.rs` (modify) | five methods, one job kind, the uid lookup |
| `polkit/org.dasbackup.policy` (modify) | `org.dasbackup.recovery-os` |
| `indexer/src/setup/installer.rs` (modify) | uninstall removes `das-recovery-os-update-*` units |
| `docs/ARCHITECTURE.md`, `docs/DISASTER-RECOVERY-GUIDE.md`, `.claude/rules/backup.md`, `CHANGELOG.md` (modify) | counts, the schedule units, the sentinel row, the entries |

---

### Task 1: `--wait-lock` for the script

**Files:**
- Modify: `scripts/recovery-os-vm.sh` (`parse_session_args` ~4636, `cmd_session` ~4716, `check_units` ~1150, `take_lock` ~2864, `cmd_session_pair` ~4290, usage text ~690, header lines 180–200)
- Test: `tests/test_recovery_os_vm.sh`

**Interfaces:**
- Produces: `session <A|B|label> [<A|B|label>] --wait-lock <minutes>` — waits up to that many minutes for the target units (`das-backup`, `das-backup-full`, `das-scrub`, `das-backup-doctor`, as `TARGET_UNITS` lists them) to be inactive AND the maintenance lock to be free, polling every `POLL_SECS`; then proceeds exactly as without the option. At the bound: `refuse` with `waited <minutes> min for …`, exit 1, nothing held. `--wait-lock` with `--dry-run` is usage (exit 2). A two-drive run passes it to neither child (the run itself waits once, before taking the lock).

- [ ] **Step 1: Write the failing tests** (append to the suite, in the section of the lock tests; follow the existing `check`/`has` style and the existing stub holder of the lock)

```bash
# --- --wait-lock (bd 8249 stage 2): a scheduled session waits for backups ---
t_wait_lock_usage() {
    local out rc=0
    out="$(DAS_RECOVERY_VM_TEST_ROOT="$ROOT" bash "$DRIVER" session A --dry-run --wait-lock 5 2>&1)" || rc=$?
    check "wait-lock with dry-run is usage" "$rc" 2
    has "wait-lock with dry-run says why" "$out" "--wait-lock has no meaning with --dry-run"
    rc=0
    out="$(DAS_RECOVERY_VM_TEST_ROOT="$ROOT" bash "$DRIVER" session A --wait-lock 0 2>&1)" || rc=$?
    check "wait-lock 0 is usage" "$rc" 2
    rc=0
    out="$(DAS_RECOVERY_VM_TEST_ROOT="$ROOT" bash "$DRIVER" session A --wait-lock x 2>&1)" || rc=$?
    check "wait-lock x is usage" "$rc" 2
}

t_wait_lock_waits_then_proceeds() {
    # Hold the maintenance lock from another process for 2 s; the session
    # must wait, then take it and run its dry-run-less preflight as usual.
    local out rc=0
    (
        exec 9<>"$ROOT/run/das-maintenance.lock"
        "$REAL_FLOCK" 9
        printf 'stub holder pid %s\n' "$BASHPID" >&9
        sleep 2
    ) &
    sleep 0.3
    out="$(DAS_RECOVERY_VM_POLL_SECS=1 DAS_RECOVERY_VM_TEST_ROOT="$ROOT" bash "$DRIVER" session A --wait-lock 1 2>&1)" || rc=$?
    wait
    has "wait-lock says it is waiting, naming the holder" "$out" "waiting for the DAS maintenance lock (held by: stub holder pid"
    has "wait-lock then takes the lock" "$out" "took the DAS maintenance lock"
    check "the session went on past the wait (full stub session exit)" "$rc" 0
}

t_wait_lock_bound() {
    local out rc=0
    (
        exec 9<>"$ROOT/run/das-maintenance.lock"
        "$REAL_FLOCK" 9
        sleep 4
    ) &
    sleep 0.3
    out="$(DAS_RECOVERY_VM_POLL_SECS=1 DAS_RECOVERY_VM_MINUTE_SECS=1 DAS_RECOVERY_VM_TEST_ROOT="$ROOT" bash "$DRIVER" session A --wait-lock 2 2>&1)" || rc=$?
    wait
    check "past the bound the session is refused" "$rc" 1
    has "the refusal says how long it waited and for what" "$out" "waited 2 min for the DAS maintenance lock"
    if ("$REAL_FLOCK" -n 9) 9<"$ROOT/run/das-maintenance.lock"; then pass "nothing held after the bound"; else fail "the lock is held after the bound"; fi
}

t_wait_lock_units() {
    # The stub systemctl reads $ROOT/stub/active-units: a listed unit is
    # "active". The session must wait for it to clear, not refuse.
    local out rc=0
    printf 'das-backup.service\n' >"$ROOT/stub/active-units"
    (sleep 2; : >"$ROOT/stub/active-units") &
    out="$(DAS_RECOVERY_VM_POLL_SECS=1 DAS_RECOVERY_VM_TEST_ROOT="$ROOT" bash "$DRIVER" session A --wait-lock 1 2>&1)" || rc=$?
    wait
    has "waits for an active backup unit" "$out" "waiting for das-backup.service (active)"
    check "goes on once it is inactive" "$rc" 0
}
```

Register each in the suite's runner the way its neighbours are (a fresh `$ROOT` per test via the existing fixture function; look at how the lock tests make a root and call `fixture_good_drive` or equivalent — copy that exactly). If the stub `systemctl` does not yet read an `active-units` file, extend the stub (it is written by the suite's fixture function; find `is-active` in it) so that a unit named in `$ROOT/stub/active-units` answers `active` and every other `inactive`.

- [ ] **Step 2: Run the suite to see the four fail**

Run: `bash tests/test_recovery_os_vm.sh 2>&1 | grep -E '^(FAIL|ok ).*wait-lock' ; bash tests/test_recovery_os_vm.sh >/dev/null 2>&1; echo rc=$?`
Expected: the `wait-lock` lines read `FAIL` (today `--wait-lock` is `-*) usage` → exit 2 everywhere), rc nonzero.

- [ ] **Step 3: Implement**

In `parse_session_args`, beside `--timeout`:

```bash
            --wait-lock)
                (($# >= 2)) || usage
                WAIT_LOCK_MIN=$2
                shift
                ;;
            --wait-lock=*) WAIT_LOCK_MIN=${1#*=} ;;
```

After the `TIMEOUT_MIN` checks:

```bash
    if [[ -n "$WAIT_LOCK_MIN" && ! "$WAIT_LOCK_MIN" =~ ^[123456789][[:digit:]]*$ ]]; then
        printf 'recovery-os-vm.sh: --wait-lock takes a whole number of minutes, at least 1\n' >&2
        exit 2
    fi
    if [[ "$DRY_RUN" == true && -n "$WAIT_LOCK_MIN" ]]; then
        printf 'recovery-os-vm.sh: --wait-lock has no meaning with --dry-run (nothing is booted, and the lock is only tried)\n' >&2
        exit 2
    fi
```

Declare `WAIT_LOCK_MIN=""` with the other option globals (near `TIMEOUT_MIN`). New function, placed before `check_units`:

```bash
# --wait-lock: wait for the target units to be inactive and the maintenance
# lock to be free, up to WAIT_LOCK_MIN minutes, looking every POLL_SECS.
# Only looks: the lock is taken by take_lock, non-blocking, as always, so a
# job that slips in between is still refused there, never raced.
wait_for_lock() {
    local deadline busy holder
    [[ -n "$WAIT_LOCK_MIN" ]] || return 0
    deadline=$((SECONDS + WAIT_LOCK_MIN * MINUTE_SECS))
    while :; do
        busy="$(busy_units)" || refuse "cannot tell whether ${TARGET_UNITS[*]} are running (systemctl said: $busy)"
        if [[ -z "$busy" ]]; then
            if (flock -n 9) 9<"$MAINTENANCE_LOCK"; then
                return 0
            fi
            holder="$(head -n 1 -- "$MAINTENANCE_LOCK" 2>/dev/null)" || holder=""
            busy="the DAS maintenance lock (held by: ${holder:-(no holder line)})"
        fi
        if ((SECONDS >= deadline)); then
            refuse "waited $WAIT_LOCK_MIN min for $busy -- nothing held; try again, or let it finish"
        fi
        log "waiting for $busy"
        sleep "$POLL_SECS"
    done
}
```

Refactor `check_units` so both share one reader: extract its loop into `busy_units` (prints `das-backup.service (active)` joined by `, `, empty when none; returns 1 with systemctl's text on stdout when the count is wrong), and have `check_units` call it and `refuse "$busy -- wait until it has finished"` when non-empty. The `MAINTENANCE_LOCK` may not exist yet on a fresh host: `wait_for_lock` treats a missing file as free (`[[ -e "$MAINTENANCE_LOCK" ]] || return 0` before the flock probe), as `take_lock` creates it.

Call `wait_for_lock` in `cmd_session` immediately before `check_units`, and in `cmd_session_pair` before its own lock taking (find where the pair run calls `take_lock`; the children get no `--wait-lock`: strip it from `flags` the way `--mode` is not passed to them — read how `flags=()` is built there).

Usage text (`usage_text`, ~line 697) and the header usage block (line ~186): add `[--wait-lock <minutes>]` to both `session` forms, and one header paragraph after the `--unattended` one:

```
#   - --wait-lock <minutes>: a scheduled session (the GUI's helper writes a
#     timer for it) may fire while a backup runs: instead of refusing at
#     once, wait up to that long for the target units to be inactive and the
#     maintenance lock to be free, looking every POLL_SECS, then go on as
#     usual (the lock is still taken without waiting: a job that slips in
#     between is refused there). Past the bound: refused, nothing held.
```

- [ ] **Step 4: Run the suite; all green**

Run: `bash tests/test_recovery_os_vm.sh 2>&1 | tail -3`
Expected: `… passed, 0 failed` with the count grown by the new checks; `shellcheck scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh` silent; `shfmt -d -i 4 -ci scripts/recovery-os-vm.sh` silent.

- [ ] **Step 5: Counter-test (observe RED)**: temporarily change `if ((SECONDS >= deadline))` to `if ((SECONDS > deadline + 100))` → `t_wait_lock_bound` must fail; restore. Record the RED line in the commit message's body.

- [ ] **Step 6: Commit**

```bash
git add scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh
git commit -S -m "$(cat <<'EOF'
recovery-os-vm.sh 2.1.0: --wait-lock for scheduled sessions

A scheduled unattended session may fire while a backup runs; --wait-lock
<minutes> waits for the target units and the maintenance lock, looking
every POLL_SECS, then goes on as usual (the lock is still taken without
waiting). Past the bound: refused, nothing held. Usage with --dry-run.
Counter-test: bound moved 100 s out -> t_wait_lock_bound RED, restored.
EOF
)"
```

---

### Task 2: `recovery_os::panel` — the pure rules and the status JSON

**Files:**
- Create: `indexer/src/recovery_os/panel.rs`
- Modify: `indexer/src/recovery_os.rs` (add `pub mod panel;` beside the existing `mod hold_disk` declaration — look at how `hold_disk` is declared and mirror it)
- Test: inline `#[cfg(test)] mod tests` in `panel.rs`

**Interfaces:**
- Consumes: `recovery_os::{StoredState, StoredDrive, RecoveryOs, GuestAgent::runs_at_boot, BootVerdict, assess, HostVersions, Assessment, load_state, state_path}`, `config::{Config, Target, TargetRole}`.
- Produces:

```rust
pub const CLEAN_RUNS_FOR_PARALLEL: u32 = 3;
pub const SCHEMA_FOR_UNATTENDED: u32 = 4;

/// Whether `label`'s record admits an unattended session, by the script's own rule
/// (check_boot_record + the --unattended checks): schema 4, a reading (not an error),
/// the guest agent installed and started at boot, verdict not `will`, and the record's
/// mount_uuid equal to the target's. Err carries the one sentence the GUI shows.
pub fn unattended_possible(target: &Target, state: &StoredState, drive: Option<&StoredDrive>) -> Result<(), String>;

/// Parallel is the everyday choice once BOTH drives have >= CLEAN_RUNS_FOR_PARALLEL
/// consecutive clean unattended runs (decision 7); `None` = count unknown.
pub fn pair_mode_default(clean_runs: &[Option<u32>]) -> &'static str; // "parallel" | "sequential"

/// A session is due when the assessment is stale by age (age_days >= max_age_days)
/// — the "update due" marker of decision 4.
pub fn due(a: &Assessment, max_age_days: u32) -> bool;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DriveStatus { pub label: String, pub display_name: String, pub serials: Vec<String>,
    pub record: Option<serde_json::Value>, pub checked_epoch: Option<i64>, pub record_error: Option<String>,
    pub assessment: Option<Assessment>, pub due: bool, pub verdict: Option<String>,
    pub unattended: Result<(), String> /* serialised as {possible: bool, why: String} */,
    pub clean_runs: Option<u32>, pub clean_runs_error: Option<String>,
    pub history: Vec<serde_json::Value>, pub schedule: Option<Schedule>, pub session: Option<Session> }

pub fn status_json(cfg: &Config, state: &Result<Option<StoredState>, String>, host: &HostVersions,
    today: &str, reads: &dyn PanelReads) -> serde_json::Value;

/// What status_json reads from the system; the helper implements it with the script and
/// systemctl, tests with a scripted one.
pub trait PanelReads {
    fn clean_runs(&self, label: &str) -> Result<u32, String>;      // `recovery-os-vm.sh clean-runs <label>`
    fn history(&self, label: &str) -> Result<Vec<serde_json::Value>, String>; // `history <label>`, newest last
    fn unit(&self, name: &str) -> Result<UnitFacts, String>;        // Task 6 defines UnitFacts
    fn domain_state(&self, label: &str) -> Option<String>;          // `virsh domstate recovery-os-updater-<label>`, None when virsh cannot say
    fn lock_holder(&self) -> Option<String>;                        // first line of /run/das-maintenance.lock if held, else None
}
```

`status_json` output shape (the GUI's contract; pin every key in a test):

```json
{"schema": 1, "max_age_days": 60, "today": "2026-10-08",
 "pair": {"mode_default": "sequential", "schedule": null, "session": null},
 "drives": [ {"label": "...", "display_name": "...", "serials": ["..."], "checked_epoch": 1791448102,
   "record_error": null, "record": {"os": "...", "installed": "...", "last_full_upgrade": "...", "kernel": "...",
     "host_kernel": "...", "btrfs_progs": "...", "host_btrfs_progs": "...", "btrbk": "...", "guest_agent": {...}},
   "assessment": {...}, "due": false, "verdict": "may",
   "unattended": {"possible": true, "why": ""}, "clean_runs": 1, "clean_runs_error": null,
   "history": [ {...last 20 lines, newest last...} ],
   "schedule": null, "session": null } ] }
```

Only `role = "mirror"` targets appear, in config order. `verdict` is `"will"|"may"|"no"|null`. `record` is the trimmed `RecoveryOs` (kernel = `newest_kernel`), `null` when the drive has no reading. `clean_runs` is `null` with `clean_runs_error` set when the script refused (a corrupt history is a refusal, never 0). `schedule` and `session` are filled in Task 6; here they are `null` and the struct fields exist with `Option<Schedule>`/`Option<Session>` placeholders defined in this task as:

```rust
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Schedule { pub unit: String, pub at_epoch: i64, pub mode: Option<String>, pub state: String /* pending|missed|fired|running */, pub detail: String }
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Session { pub by: String /* job:<id> | unit:<name> | other:<holder line> */, pub since_epoch: Option<i64>, pub domain_state: Option<String>, pub attended: Option<bool> }
```

- [ ] **Step 1: Write the failing tests** (`panel.rs`, `mod tests`; build fixtures with the crate's existing test helpers in `recovery_os.rs` — `tests::fixture` functions that write an OS root and a record; read `the_guest_agent_installed_with_its_udev_rule_runs_at_boot` (~line 4571) for the agent fixture and `write_state`'s tests for a stored record)

```rust
#[test]
fn unattended_needs_schema_4_a_reading_the_agent_at_boot_not_will_and_the_same_filesystem() {
    let target = mirror_target("system-recovery-A-2tb", Some("60b05268-7f8f-47b5-a38a-752576a1172a"));
    let good = stored_drive(agent_at_boot(), BootVerdict::May, Some("60b05268-7f8f-47b5-a38a-752576a1172a"));
    let state = state_v(4, &[("system-recovery-A-2tb", good.clone())]);
    assert_eq!(unattended_possible(&target, &state, state.drives.get(&target.label)), Ok(()));

    let v3 = state_v(3, &[("system-recovery-A-2tb", good.clone())]);
    assert!(unattended_possible(&target, &v3, v3.drives.get(&target.label)).unwrap_err().contains("schema 4"));
    assert!(unattended_possible(&target, &state, None).unwrap_err().contains("no record"));

    let mut err = good.clone(); err.os = None; err.error = Some("could not read".into());
    assert!(unattended_possible(&target, &state, Some(&err)).unwrap_err().contains("could not read"));

    let no_agent = stored_drive(agent_not_installed(), BootVerdict::May, good.mount_uuid.clone());
    assert!(unattended_possible(&target, &state, Some(&no_agent)).unwrap_err().contains("guest agent"));

    let will = stored_drive(agent_at_boot(), BootVerdict::Will, good.mount_uuid.clone());
    assert!(unattended_possible(&target, &state, Some(&will)).unwrap_err().contains("will run btrbk"));

    let other_fs = stored_drive(agent_at_boot(), BootVerdict::May, Some("7c7ae72d-09d6-4086-b249-1ac60f21b73b".into()));
    assert!(unattended_possible(&target, &state, Some(&other_fs)).unwrap_err().contains("filesystem"));
    let no_uuid = stored_drive(agent_at_boot(), BootVerdict::May, None);
    assert!(unattended_possible(&target, &state, Some(&no_uuid)).unwrap_err().contains("filesystem"));
    let target_no_uuid = mirror_target("system-recovery-A-2tb", None);
    assert!(unattended_possible(&target_no_uuid, &state, Some(&good)).unwrap_err().contains("mount_uuid"));
}

#[test]
fn parallel_is_the_default_only_once_both_drives_have_three_clean_runs() {
    assert_eq!(pair_mode_default(&[Some(3), Some(3)]), "parallel");
    assert_eq!(pair_mode_default(&[Some(7), Some(3)]), "parallel");
    assert_eq!(pair_mode_default(&[Some(3), Some(2)]), "sequential");
    assert_eq!(pair_mode_default(&[None, Some(9)]), "sequential");
    assert_eq!(pair_mode_default(&[]), "sequential");
    assert_eq!(pair_mode_default(&[Some(3)]), "sequential"); // one drive is not both
}

#[test]
fn due_is_stale_by_age_not_by_any_other_reason() {
    let by_age = Assessment { age_days: Some(61), stale: true, reasons: vec!["last applied full upgrade 61 days ago".into()], ..Default::default() };
    assert!(due(&by_age, 60));
    let by_kernel = Assessment { age_days: Some(3), stale: true, reasons: vec!["kernel series behind".into()], ..Default::default() };
    assert!(!due(&by_kernel, 60));
    let unknown = Assessment { age_days: None, stale: true, ..Default::default() };
    assert!(!due(&unknown, 60), "an unknown age is not 'due' (it is reported stale, not scheduled)");
    assert!(due(&Assessment { age_days: Some(60), stale: true, ..Default::default() }, 60));
}

#[test]
fn status_json_has_every_key_the_gui_reads_and_only_mirror_targets() {
    let cfg = two_mirrors_and_a_primary();
    let state = Ok(Some(state_v(4, &[("system-recovery-A-2tb", stored_drive(agent_at_boot(), BootVerdict::May, Some(uuid_a())))])));
    let reads = Scripted { clean: [("system-recovery-A-2tb", Ok(2)), ("system-recovery-B-2tb", Err("history cannot be read".into()))].into(),
                           history: [("system-recovery-A-2tb", Ok(vec![serde_json::json!({"label":"system-recovery-A-2tb","outcome":"clean"})]))].into(), ..Default::default() };
    let j = status_json(&cfg, &state, &host(), "2026-10-08", &reads);
    assert_eq!(j["schema"], 1);
    assert_eq!(j["drives"].as_array().unwrap().len(), 2, "primary-22tb is not a drive here");
    let a = &j["drives"][0];
    for key in ["label","display_name","serials","checked_epoch","record_error","record","assessment","due","verdict","unattended","clean_runs","clean_runs_error","history","schedule","session"] {
        assert!(a.get(key).is_some(), "missing {key}: {a}");
    }
    assert_eq!(a["verdict"], "may");
    assert_eq!(a["unattended"]["possible"], true);
    assert_eq!(a["clean_runs"], 2);
    assert_eq!(a["history"].as_array().unwrap().len(), 1);
    let b = &j["drives"][1];
    assert!(b["record"].is_null() && b["checked_epoch"].is_null(), "B has no reading");
    assert_eq!(b["unattended"]["possible"], false);
    assert!(b["clean_runs"].is_null(), "a refused count is null, never 0");
    assert_eq!(b["clean_runs_error"], "history cannot be read");
    assert_eq!(j["pair"]["mode_default"], "sequential");
}

#[test]
fn status_json_with_an_unreadable_state_file_says_so_on_every_drive() {
    let cfg = two_mirrors_and_a_primary();
    let state: Result<Option<StoredState>, String> = Err("/var/lib/das-backup/recovery-os.json: corrupt".into());
    let j = status_json(&cfg, &state, &host(), "2026-10-08", &Scripted::default());
    for d in j["drives"].as_array().unwrap() {
        assert!(d["record_error"].as_str().unwrap().contains("corrupt"));
        assert_eq!(d["unattended"]["possible"], false);
    }
}
```

Write the fixture helpers (`mirror_target`, `stored_drive`, `agent_at_boot`, `agent_not_installed`, `state_v`, `two_mirrors_and_a_primary`, `host`, `uuid_a`, `Scripted` implementing `PanelReads` with `HashMap`s and `Default`) in the same test module; `stored_drive` builds a `RecoveryOs` by `Default` then sets `guest_agent` and `btrbk_at_boot.verdict` (look at `BtrbkAtBoot`'s fields ~line 181).

- [ ] **Step 2: Run to see them fail to compile**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-8249-s2-target cargo test panel:: 2>&1 | tail -5`
Expected: compile error, `panel` unresolved.

- [ ] **Step 3: Implement `panel.rs`**

```rust
//! The recovery-drive panel's rules and its status document (bd 8249 stage 2).
//! Pure: everything the system says comes through [`PanelReads`].
use std::collections::BTreeMap;
use serde::Serialize;
use crate::config::{Config, Target, TargetRole};
use crate::recovery_os::{assess, newest_kernel, Assessment, BootVerdict, HostVersions, StoredDrive, StoredState};

pub const CLEAN_RUNS_FOR_PARALLEL: u32 = 3;
pub const SCHEMA_FOR_UNATTENDED: u32 = 4;

pub fn unattended_possible(target: &Target, state: &StoredState, drive: Option<&StoredDrive>) -> Result<(), String> {
    if state.schema_version < SCHEMA_FOR_UNATTENDED {
        return Err(format!("the boot record is schema {}; an unattended session needs schema 4 (let a backup run record the drive again)", state.schema_version));
    }
    let Some(target_uuid) = target.mount_uuid.as_deref() else {
        return Err(format!("{} has no mount_uuid in config.toml; the session cannot tie the record to the filesystem", target.label));
    };
    let Some(d) = drive else { return Err(format!("no record of {} (let a backup run record it)", target.label)); };
    if let Some(e) = &d.error { return Err(format!("the last reading of {} failed: {e}", target.label)); }
    let Some(os) = &d.os else { return Err(format!("the record of {} holds no reading", target.label)); };
    match d.mount_uuid.as_deref() {
        Some(u) if u == target_uuid => {}
        Some(u) => return Err(format!("the record of {} is of filesystem {u}, config mounts {target_uuid}", target.label)),
        None => return Err(format!("the record of {} names no filesystem (made before df0); let a backup run record it again", target.label)),
    }
    if !os.guest_agent.runs_at_boot() {
        return Err(format!("the guest agent does not start at boot in {}'s OS ({}); run an attended session and install/enable qemu-guest-agent", target.label, agent_words(&os.guest_agent)));
    }
    if os.btrbk_at_boot.verdict == BootVerdict::Will {
        return Err(format!("{}'s OS will run btrbk at boot; an attended session must disable it first", target.label));
    }
    Ok(())
}
```

`agent_words` formats `GuestAgent` (match its variants — read the enum at ~line 276 — into `installed, enabled`/`not installed`/`unreadable: <reason>`). `pair_mode_default`: `if clean.len() == 2 && clean.iter().all(|c| c.is_some_and(|n| n >= CLEAN_RUNS_FOR_PARALLEL)) { "parallel" } else { "sequential" }`. `due`: `a.age_days.is_some_and(|d| d >= i64::from(max_age_days))`. `status_json`: iterate `cfg.targets` with `role == TargetRole::Mirror`, per target build `DriveStatus` (assess with `assess(os, host, today, cfg.recovery_os.max_age_days)`; `record` trimmed via a small `record_value(os, host)`; history = last 20 of `reads.history(label)`; `clean_runs`/`clean_runs_error` from `reads.clean_runs`), then `serde_json::to_value`. Serialise `unattended` by a custom `#[serde(serialize_with)]` or by building the JSON object by hand (`{"possible": r.is_ok(), "why": r.err().unwrap_or_default()}`) — building by hand is simpler; do that for the whole `DriveStatus` and keep the struct as the typed intermediate. `pair.mode_default = pair_mode_default(&[a.clean_runs, b.clean_runs])` over the first two mirrors (one mirror: `"sequential"`).

- [ ] **Step 4: Run the tests; green**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-8249-s2-target cargo test panel:: 2>&1 | tail -3`
Expected: `test result: ok. 5 passed`.

- [ ] **Step 5: Counter-test**: flip `>= CLEAN_RUNS_FOR_PARALLEL` to `>` → `parallel_is_the_default…` RED; restore. Clippy + fmt clean.

- [ ] **Step 6: Commit** — `git add indexer/src/recovery_os/panel.rs indexer/src/recovery_os.rs && git commit -S -m "recovery_os::panel: the drive panel's rules and status document (8249 stage 2)"` with a body naming the RED observed.

---

### Task 3: the session line parser

**Files:**
- Modify: `indexer/src/recovery_os/panel.rs` (add the parser; it belongs with the panel's vocabulary) — or a sibling `lines.rs` if `panel.rs` passes ~600 lines; keep one.
- Test: inline.

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLine<'a> {
    Progress { label: &'a str, step: &'a str, event: &'a str, message: &'a str }, // event: start|ok|fail
    Output   { label: &'a str, step: &'a str, text: &'a str },
    Result   { label: &'a str, exit: i32, outcome: &'a str },
    Drive    { label: &'a str, exit: Option<i32> }, // None = "skipped"
    Other(&'a str),
}
pub fn parse_session_line(line: &str) -> SessionLine<'_>;
pub const STEPS: [&str; 18] = ["preflight","start","wait","boot","egress","snapshot","guard-lift","keyrings","upgrade","packages","initramfs","verify-btrbk","verify-boot","guard-engage","reboot","kernel","poweroff","giveback"];
/// The step's position for a percent (0..=100): preflight 0 … giveback 100; an unknown step is None.
pub fn step_percent(step: &str) -> Option<i32>;
```

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn the_four_machine_lines_parse_and_everything_else_is_other() {
    assert_eq!(parse_session_line("PROGRESS system-recovery-A-2tb upgrade start"),
        SessionLine::Progress { label: "system-recovery-A-2tb", step: "upgrade", event: "start", message: "" });
    assert_eq!(parse_session_line("PROGRESS system-recovery-A-2tb egress fail the public address belongs to AS207990 HostRoyale"),
        SessionLine::Progress { label: "system-recovery-A-2tb", step: "egress", event: "fail", message: "the public address belongs to AS207990 HostRoyale" });
    assert_eq!(parse_session_line("OUTPUT system-recovery-A-2tb upgrade :: Proceed with installation? [Y/n]"),
        SessionLine::Output { label: "system-recovery-A-2tb", step: "upgrade", text: ":: Proceed with installation? [Y/n]" });
    assert_eq!(parse_session_line("RESULT system-recovery-B-2tb 5 warnings"), SessionLine::Result { label: "system-recovery-B-2tb", exit: 5, outcome: "warnings" });
    assert_eq!(parse_session_line("DRIVE system-recovery-B-2tb skipped"), SessionLine::Drive { label: "system-recovery-B-2tb", exit: None });
    assert_eq!(parse_session_line("DRIVE system-recovery-A-2tb 0"), SessionLine::Drive { label: "system-recovery-A-2tb", exit: Some(0) });
    for other in ["", "took the DAS maintenance lock", "PROGRESS", "PROGRESS x", "RESULT x notanumber clean", "progress a b c", "DRIVE x -1x"] {
        assert_eq!(parse_session_line(other), SessionLine::Other(other), "{other:?}");
    }
}

#[test]
fn unknown_lines_are_logged_not_lost() {
    // Review focus 4: a PROGRESS line with a step not in STEPS still parses (the GUI logs it);
    // only step_percent is None.
    assert!(matches!(parse_session_line("PROGRESS system-recovery-A-2tb newstep ok"), SessionLine::Progress { step: "newstep", .. }));
    assert_eq!(step_percent("newstep"), None);
    assert_eq!(step_percent("preflight"), Some(0));
    assert_eq!(step_percent("giveback"), Some(100));
    assert_eq!(step_percent("upgrade"), Some(8 * 100 / 17));
}
```

- [ ] **Step 2: Run; fail to compile.** `cargo test panel::tests::the_four`.

- [ ] **Step 3: Implement** with `split_whitespace`/`splitn` on the fixed prefixes; `RESULT` needs exactly a parsable `i32` exit; `DRIVE` takes `skipped` or an `i32`; a line that does not fit is `Other(line)` whole. `step_percent`: `STEPS.iter().position(..).map(|i| (i as i32) * 100 / (STEPS.len() as i32 - 1))`.

- [ ] **Step 4: Run; green.** **Step 5:** counter-test: make `RESULT` with a bad exit parse as exit 0 → the `Other` loop RED; restore. **Step 6: Commit** `recovery_os::panel: parse the script's PROGRESS/OUTPUT/RESULT/DRIVE lines`.

---

### Task 4: `recovery_os::session` — spawning the script as a job

**Files:**
- Create: `indexer/src/recovery_os/session.rs`
- Modify: `indexer/src/recovery_os.rs` (`pub mod session;`)
- Test: inline, driving a stub script written to a tempdir.

**Interfaces:**
- Consumes: `panel::{parse_session_line, SessionLine, step_percent}`, `progress::{ProgressSink-driven OrderedProgress? — no: take `&dyn Progress`}`. Read `indexer/src/progress.rs` fully first: the job uses the same `Progress` trait the backup job uses (`stage`, `progress`, `log`, and `stop_requested` via `progress::stop_requested`).
- Produces:

```rust
pub const SCRIPT: &str = "/usr/lib/das-backup/recovery-os-vm.sh";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRequest { pub labels: Vec<String> /* 1 or 2, validated */, pub unattended: bool,
    pub mode: Option<String> /* Some only with 2 labels: "sequential"|"parallel" */, pub accept_boot_record_risk: bool }

impl SessionRequest {
    /// Refuses before anything runs: no label, more than two, a repeated one, a label config
    /// does not list as role mirror, a mode with one label or an unknown mode.
    pub fn validate(&self, cfg: &Config) -> Result<(), String>;
    /// The script's argument vector: session <l1> [<l2>] [--unattended] [--mode m] [--accept-boot-record-risk]
    pub fn args(&self) -> Vec<String>;
}

/// The environment the script gets: built, never inherited (no DAS_RECOVERY_* seam reachable).
pub fn script_command(script: &Path, args: &[String]) -> Command;

/// How the job starts the script; the helper uses `SystemSpawner`, tests a stub.
pub trait SessionSpawner { fn spawn(&self, cmd: Command) -> io::Result<Box<dyn SessionChild>>; }
pub trait SessionChild {
    fn pid(&self) -> u32;
    /// Each stdout line as it arrives (stderr lines too, prefixed by the implementation with "stderr: ").
    fn for_each_line(&mut self, on_line: &mut dyn FnMut(&str)) -> io::Result<()>;
    fn wait(&mut self) -> io::Result<ExitStatus>;
    fn interrupt(&self) -> io::Result<()>; // SIGINT to the pid, once
}
pub struct SystemSpawner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOutcome { pub exit: Option<i32> /* None: killed by a signal */, pub results: Vec<(String, i32, String)>, pub drives: Vec<(String, Option<i32>)>, pub last_lines: Vec<String> /* the last 12 human lines */ }
impl SessionOutcome {
    pub fn success(&self) -> bool { self.exit == Some(0) }
    /// "clean" | "warnings: …" | "kept (exit 3): …" | "failed (exit N): …" | "stopped at <step> (exit 7): …" | "killed by signal: …"
    pub fn summary(&self) -> String;
}

/// Runs the script to its end, mapping lines to progress: PROGRESS → stage "<label>:<step>" and
/// percent; OUTPUT → log Info "[<label>] <step>: <text>"; RESULT/DRIVE → recorded + log; Other → log Info.
/// If `stop_requested(progress)` becomes true, `interrupt()` once and keep reading to the end.
pub fn run_session(spawner: &dyn SessionSpawner, script: &Path, cfg: &Config, req: &SessionRequest, progress: &dyn Progress) -> Result<SessionOutcome, String>;
```

- [ ] **Step 1: Failing tests** (a stub script in a tempdir that prints a scripted sequence and exits with a chosen status; `env` printed on demand to prove the environment)

```rust
fn stub_script(dir: &Path, body: &str) -> PathBuf { let p = dir.join("recovery-os-vm.sh"); std::fs::write(&p, format!("#!/bin/bash\n{body}\n")).unwrap(); std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap(); p }

#[test]
fn a_request_is_validated_before_anything_runs() {
    let cfg = two_mirrors_and_a_primary(); // reuse panel's fixture: make it pub(crate) in panel::tests or duplicate it here
    let ok = SessionRequest { labels: vec!["system-recovery-A-2tb".into()], unattended: false, mode: None, accept_boot_record_risk: false };
    assert_eq!(ok.validate(&cfg), Ok(()));
    let bad = |labels: &[&str], mode: Option<&str>| SessionRequest { labels: labels.iter().map(|s| s.to_string()).collect(), unattended: true, mode: mode.map(String::from), accept_boot_record_risk: false }.validate(&cfg).unwrap_err();
    assert!(bad(&[], None).contains("no drive"));
    assert!(bad(&["primary-22tb"], None).contains("not a recovery drive"));
    assert!(bad(&["nope"], None).contains("not in the configuration"));
    assert!(bad(&["system-recovery-A-2tb","system-recovery-A-2tb"], None).contains("twice"));
    assert!(bad(&["system-recovery-A-2tb","system-recovery-B-2tb","system-recovery-A-2tb"], None).contains("at most two"));
    assert!(bad(&["system-recovery-A-2tb"], Some("parallel")).contains("two drives"));
    assert!(bad(&["system-recovery-A-2tb","system-recovery-B-2tb"], Some("fast")).contains("sequential or parallel"));
    assert_eq!(SessionRequest { labels: vec!["system-recovery-A-2tb".into(), "system-recovery-B-2tb".into()], unattended: true, mode: Some("parallel".into()), accept_boot_record_risk: true }.args(),
        ["session","system-recovery-A-2tb","system-recovery-B-2tb","--unattended","--mode","parallel","--accept-boot-record-risk"]);
}

#[test]
fn the_script_runs_in_a_built_environment_with_no_seam_reachable() {
    let dir = tempfile::tempdir().unwrap();
    let script = stub_script(dir.path(), "env | sort; exit 0");
    // Poison the parent's environment the way a misconfigured unit might.
    unsafe { std::env::set_var("DAS_RECOVERY_VM_TEST_ROOT", "/tmp/poison"); std::env::set_var("DAS_RECOVERY_OS_STATE", "/tmp/poison.json"); std::env::set_var("DAS_CONFIG", "/tmp/poison.toml"); std::env::set_var("BTRDASD_BIN", "/tmp/poison-bin"); }
    let sink = CapturingProgress::default();
    let out = run_session(&SystemSpawner, &script, &two_mirrors_and_a_primary(), &one_drive(), &sink).unwrap();
    assert_eq!(out.exit, Some(0));
    let env = sink.logs().join("\n");
    for v in ["DAS_RECOVERY", "DAS_CONFIG", "BTRDASD_BIN"] { assert!(!env.contains(v), "{v} reached the script:\n{env}"); }
    assert!(env.contains("LC_ALL=C") && env.contains("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin") && env.contains("HOME=/root"));
    unsafe { for v in ["DAS_RECOVERY_VM_TEST_ROOT","DAS_RECOVERY_OS_STATE","DAS_CONFIG","BTRDASD_BIN"] { std::env::remove_var(v); } }
}

#[test]
fn lines_become_stages_logs_results_and_the_exit_is_the_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let script = stub_script(dir.path(), r#"
echo "took the DAS maintenance lock"
echo "PROGRESS system-recovery-A-2tb preflight ok lock taken; unattended, single"
echo "PROGRESS system-recovery-A-2tb upgrade start"
echo "OUTPUT system-recovery-A-2tb upgrade :: Starting full system upgrade..."
echo "PROGRESS system-recovery-A-2tb upgrade fail pacman exited 1"
echo "RESULT system-recovery-A-2tb 7 failed"
exit 7"#);
    let sink = CapturingProgress::default();
    let out = run_session(&SystemSpawner, &script, &two_mirrors_and_a_primary(), &one_drive(), &sink).unwrap();
    assert_eq!(out.exit, Some(7));
    assert_eq!(out.results, vec![("system-recovery-A-2tb".to_string(), 7, "failed".to_string())]);
    assert!(sink.stages().contains(&"system-recovery-A-2tb:upgrade".to_string()));
    assert!(sink.logs().iter().any(|l| l.contains(":: Starting full system upgrade")));
    assert!(sink.logs().iter().any(|l| l == "took the DAS maintenance lock"), "human lines are logged, not lost");
    assert!(!out.success());
    let s = out.summary();
    assert!(s.starts_with("stopped at upgrade (exit 7)"), "{s}");
    assert!(s.contains("pacman exited 1"));
}

#[test]
fn exit_5_is_not_success_and_the_summary_begins_with_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let script = stub_script(dir.path(), "echo 'RESULT system-recovery-A-2tb 5 warnings'; echo 'WARNING: the claim was lost while the VM ran'; exit 5");
    let out = run_session(&SystemSpawner, &script, &two_mirrors_and_a_primary(), &one_drive(), &CapturingProgress::default()).unwrap();
    assert!(!out.success());
    assert!(out.summary().starts_with("warnings"), "{}", out.summary());
    assert!(out.summary().contains("claim was lost"));
    let script = stub_script(dir.path(), "echo 'RESULT system-recovery-A-2tb 0 clean'; exit 0");
    let out = run_session(&SystemSpawner, &script, &two_mirrors_and_a_primary(), &one_drive(), &CapturingProgress::default()).unwrap();
    assert!(out.success()); assert_eq!(out.summary(), "clean");
    let script = stub_script(dir.path(), "echo 'RESULT system-recovery-A-2tb 3 kept'; exit 3");
    let out = run_session(&SystemSpawner, &script, &two_mirrors_and_a_primary(), &one_drive(), &CapturingProgress::default()).unwrap();
    assert!(out.summary().starts_with("kept (exit 3)"), "{}", out.summary());
}

#[test]
fn a_stop_request_interrupts_once_and_the_job_ends_with_the_scripts_exit() {
    let dir = tempfile::tempdir().unwrap();
    // The stub traps INT like the driver: says so, exits 3 (VM left running).
    let script = stub_script(dir.path(), r#"
trap 'echo "interrupted: the recovery OS is still running; finish with session-end"; exit 3' INT
echo "PROGRESS system-recovery-A-2tb wait start"
for i in $(seq 1 100); do sleep 0.1; done
exit 0"#);
    let sink = CapturingProgress::default();
    sink.cancel_after_first_stage();
    let out = run_session(&SystemSpawner, &script, &two_mirrors_and_a_primary(), &one_drive(), &sink).unwrap();
    assert_eq!(out.exit, Some(3));
    assert!(out.summary().contains("session-end"));
    assert_eq!(sink.interrupts_seen(), 1); // CapturingProgress counts stop_requested polls that returned true after the first; or assert via a stub SessionSpawner wrapping SystemSpawner that counts interrupt()
}

#[test]
fn a_script_that_cannot_start_is_an_error_not_an_outcome() {
    let err = run_session(&SystemSpawner, Path::new("/nonexistent/recovery-os-vm.sh"), &two_mirrors_and_a_primary(), &one_drive(), &CapturingProgress::default()).unwrap_err();
    assert!(err.contains("/nonexistent/recovery-os-vm.sh"));
}
```

`one_drive()` is `SessionRequest { labels: vec!["system-recovery-A-2tb".into()], unattended: false, mode: None, accept_boot_record_risk: false }`; `run_session` with a stub script skips nothing — it validates against a `Config` it is given, so give `run_session` a `cfg: &Config` parameter (add it to the interface above) and pass `two_mirrors_and_a_primary()`. `CapturingProgress` implements the crate's `Progress` trait recording stages and logs behind a `Mutex`, with a cancel token that flips after the first `stage` call (look at how `backup.rs`'s tests implement cancellation — `CancelDuringSnapshot` ~line 8830 — and copy the token mechanism). For the interrupt count, wrap `SystemSpawner` in a test `CountingSpawner` whose child delegates and counts `interrupt()`.

- [ ] **Step 2: Run; fail to compile.**

- [ ] **Step 3: Implement.** `SystemSpawner::spawn`: `cmd.stdout(piped).stderr(piped).spawn()`, child wraps `std::process::Child`; `for_each_line` reads stdout on the calling thread and stderr on a helper thread (same hazard `fsutil::SystemRunner::stream` documents: drain both), stderr lines delivered as `stderr: <line>`; `interrupt` = `nix`-free: `Command::new("kill").args(["-INT", &pid])` is NOT acceptable (a second process to signal a first); use `libc::kill(pid as i32, libc::SIGINT)` — check `Cargo.toml` for `libc` (rusqlite pulls it; add `libc = "0.2"` to `[dependencies]` if not direct). `run_session`: validate is the caller's (helper) job but call it again here (cheap, and the library must not trust callers); build the command with `script_command`; poll `progress::stop_requested(progress)` in the line loop and on a 250 ms tick when no line arrives (the stub stdout read blocks; use the stderr thread's channel or a `recv_timeout` loop over a line channel — implement `for_each_line` by feeding both streams into one `mpsc` channel and have `run_session` `recv_timeout(250ms)` so cancel is seen without a line). `summary()` per the interface's words; `stopped at <step>` comes from the last `Progress { event: "fail" }` step when exit is 7.

- [ ] **Step 4: Run; green** (`cargo test session::`). **Step 5:** counter-test: remove `env_clear()` → `…no_seam_reachable` RED; restore. **Step 6: Commit** `recovery_os::session: run recovery-os-vm.sh as a job (built environment, SIGINT cancel)`.

---

### Task 5: the helper's five methods and the polkit action

**Files:**
- Modify: `indexer/src/bin/btrdasd-helper.rs` (methods after `health_query`; `JobMap` value gains the session child's pid? No — cancel goes through `progress.cancel()` as every job does, and `run_session` reads it; nothing new in the map).
- Modify: `polkit/org.dasbackup.policy` (new action), `dbus/org.dasbackup.Helper1.conf` unchanged.
- Test: `#[cfg(test)]` in the helper (pure parts) + `every_method…` style source-scan test.

**Interfaces** (D-Bus, interface `org.dasbackup.Helper1`):

| Method | Args → Return | Polkit |
|---|---|---|
| `RecoveryOsStatus()` | → `s` JSON (`panel::status_json`) | `org.dasbackup.health` |
| `RecoveryOsSession(labels: as, unattended: b, mode: s, accept_boot_record_risk: b)` | → `s` job id; refused (`fdo::Error::Failed`) when a session job is already running or a `das-recovery-os-update-*` service is active, naming it; `InvalidArgs` on `SessionRequest::validate` errors | `org.dasbackup.recovery-os` |
| `RecoveryOsSessionEnd(label: s)` | → `(b, s)` = (exit 0, the script's last lines) | `org.dasbackup.recovery-os` |
| `RecoveryOsConsole(label: s)` | → `s` socket path; the uid is the CALLER's, from `org.freedesktop.DBus.GetConnectionUnixUser(sender)`; a failed lookup is `fdo::Error::Failed`, never uid 0 | `org.dasbackup.recovery-os` |
| `RecoveryOsScheduleSet(labels: as, at_epoch: x, mode: s)` | → `s` unit name; `at_epoch == 0` clears the schedule for those labels. **Added in Task 6, not here**: this task ends with four methods | `org.dasbackup.recovery-os` |

Session job: `tokio::task::spawn_blocking(move || session::run_session(&session::SystemSpawner, Path::new(session::SCRIPT), &req, &*progress))`, then `finish_job(progress, out.success(), out.summary())` — on `Err(e)`: `finish_job(progress, false, e)`.

- [ ] **Step 1: Failing tests** (helper `mod tests`)

```rust
#[test]
fn the_recovery_os_methods_check_polkit_with_the_recovery_os_action() {
    let src = include_str!("btrdasd-helper.rs");
    let body = &src[..src.find("#[cfg(test)]").unwrap()];
    for m in ["async fn recovery_os_session(", "async fn recovery_os_session_end(", "async fn recovery_os_console("] {
        let at = body.find(m).unwrap_or_else(|| panic!("{m} missing"));
        let after = &body[at..at + 1200];
        assert!(after.contains("check_polkit(&self.conn, &sender, \"org.dasbackup.recovery-os\")"), "{m} does not check org.dasbackup.recovery-os");
    }
    let at = body.find("async fn recovery_os_status(").unwrap();
    assert!(body[at..at + 600].contains("\"org.dasbackup.health\""));
}

#[test]
fn a_second_session_job_is_refused_naming_the_first() {
    // Pure helper: session_busy(jobs: &[(&str /*id*/, &str /*kind*/)], units: &[(&str, &str /*ActiveState*/)]) -> Option<String>
    assert_eq!(session_busy(&[("job-1", "backup")], &[]), None);
    assert_eq!(session_busy(&[("job-2", "recovery-os-session")], &[]).unwrap(), "a recovery-OS session is already running as job job-2");
    assert_eq!(session_busy(&[], &[("das-recovery-os-update-both.service", "activating")]).unwrap(), "a scheduled recovery-OS session is running: das-recovery-os-update-both.service (activating)");
    assert_eq!(session_busy(&[], &[("das-recovery-os-update-system-recovery-A-2tb.service", "inactive")]), None);
}

#[test]
fn the_polkit_policy_declares_the_recovery_os_action_as_auth_admin_keep() {
    let policy = include_str!("../../../polkit/org.dasbackup.policy");
    let at = policy.find("action id=\"org.dasbackup.recovery-os\"").expect("action declared");
    let block = &policy[at..policy[at..].find("</action>").unwrap() + at];
    assert!(block.contains("<allow_any>no</allow_any>") && block.contains("<allow_active>auth_admin_keep</allow_active>"), "{block}");
}
```

For the job kind, the `JobMap` value is `(JoinHandle, Arc<OrderedProgress>, String /*owner*/)`; add a fourth element `&'static str` kind (`"backup"`, `"index"`, `"restore"`, `"recovery-os-session"`) — update every `insert` and the `job_cancel` match (which destructures a 3-tuple; make it `Some((_, progress, _, _))`). `session_busy` takes the kinds and the `systemctl show` facts the helper read (`systemctl show -P ActiveState <unit>` for `das-recovery-os-update-both.service` and one per mirror label).

- [ ] **Step 2: Run** `cargo test --features dbus the_recovery_os_methods` → fails (names missing).

- [ ] **Step 3: Implement.** Method skeleton (status):

```rust
    /// The recovery-drive panel's document (bd DAS-Backup-Manager-8249).
    async fn recovery_os_status(&self, #[zbus(header)] header: zbus::message::Header<'_>) -> fdo::Result<String> {
        let sender = sender_from_header(&header)?;
        check_polkit(&self.conn, &sender, "org.dasbackup.health").await?;
        let config = load_config()?;
        let json = tokio::task::spawn_blocking(move || {
            let state = recovery_os::load_state(&recovery_os::state_path());
            let host = recovery_os::host_versions();
            let today = recovery_os::today_local(); // add if absent: find how `recovery-os status` computes `today` in main.rs and move it into the library
            let reads = SystemPanelReads { script: PathBuf::from(session::SCRIPT) };
            recovery_os::panel::status_json(&config, &state, &host, &today, &reads).to_string()
        }).await.map_err(|e| fdo::Error::Failed(format!("status task panicked: {e}")))?;
        Ok(json)
    }
```

`SystemPanelReads` (in the helper file, above the interface) implements `PanelReads`: `clean_runs` runs `script clean-runs <label>` through `script_command` and parses stdout as `u32` (anything else → `Err(stderr or stdout)`); `history` runs `script history <label>` and parses each line as JSON (a bad line → `Err`); `unit` — Task 6; `domain_state` runs `virsh domstate recovery-os-updater-<label>` (trim; nonzero → `None`); `lock_holder` reads the first line of `/run/das-maintenance.lock` only if `flock -n` on it fails (use `buttered_dasd::scrub::FileLock::try_acquire` → `Ok(None)` means held → read line). Console: `let uid = zbus::fdo::DBusProxy::new(&self.conn).await?.get_connection_unix_user(sender.as_str().try_into()?).await.map_err(|e| fdo::Error::Failed(format!("cannot name the caller's uid: {e}")))?;` then run `script console-socket <label> <uid>`; stdout's single line is the path; nonzero → `fdo::Error::Failed(stderr)`. Session-end: run `script session-end <label>`, return `(status.success(), last 12 lines joined)`.

Add to `polkit/org.dasbackup.policy` after the `backup` action:

```xml
  <action id="org.dasbackup.recovery-os">
    <description>Boot a recovery drive's own OS in its update VM, and schedule it</description>
    <message>Authentication is required to start or schedule a recovery-OS update session</message>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
  </action>
```

- [ ] **Step 4:** `cargo test --features dbus` green; `cargo clippy --all-targets --features dbus -- -D warnings` clean. **Step 5:** counter-test: change the console method's action string to `org.dasbackup.health` → source-scan test RED; restore. **Step 6: Commit** `btrdasd-helper: RecoveryOsStatus, RecoveryOsSession, RecoveryOsSessionEnd, RecoveryOsConsole; polkit org.dasbackup.recovery-os`.

---

### Task 6: schedules — generated unit pairs, their reading, and `RecoveryOsScheduleSet`

**Files:**
- Modify: `indexer/src/recovery_os/panel.rs` (renderer + reader; the `Schedule`/`Session` fillers in `status_json`)
- Modify: `indexer/src/bin/btrdasd-helper.rs` (`RecoveryOsScheduleSet`, `SystemPanelReads::unit`)
- Modify: `indexer/src/setup/installer.rs` (`uninstall_with` removes `/etc/systemd/system/das-recovery-os-update-*.{service,timer}` after `disable --now` of each timer found)
- Test: inline + installer tests.

**Interfaces:**

```rust
pub const UNIT_PREFIX: &str = "das-recovery-os-update-";
pub fn unit_base(labels: &[String]) -> String; // one label → UNIT_PREFIX + label; two → UNIT_PREFIX + "both"
pub fn render_service(labels: &[String], mode: Option<&str>, script: &Path) -> String;
pub fn render_timer(labels: &[String], at_epoch: i64) -> String; // OnCalendar=YYYY-MM-DD HH:MM:SS in LOCAL time, Persistent=false
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UnitFacts { pub exists: bool, pub active_state: String, pub next_elapse_epoch: Option<i64>, pub last_trigger_epoch: Option<i64>, pub result: Option<String> /* service Result= */, pub exec_main_status: Option<i32>, pub exec_main_start_epoch: Option<i64>, pub on_calendar: Option<String> /* read from the file */, pub last_journal: Vec<String> /* last 5 lines */ }
/// pending (timer active, next elapse in the future) | running (service active/activating) | fired (service inactive, last trigger set: detail = history outcome if the newest history line's start >= last trigger, else "refused: <last journal lines>") | missed (timer active, no last trigger, on_calendar time < now) 
pub fn schedule_state(timer: &UnitFacts, service: &UnitFacts, history_newest: Option<&serde_json::Value>, now_epoch: i64) -> Schedule;
pub fn validate_schedule(cfg: &Config, state: &StoredState, labels: &[String], at_epoch: i64, mode: Option<&str>, now_epoch: i64) -> Result<(), String>; // future by >= 120 s; every label unattended_possible; mode rules as SessionRequest; a drive already in the other kind of schedule (per-drive vs both) is refused "clear it first"
```

Session filler: `Session { by: "job:<id>" }` is the helper's (it knows its jobs; pass the running job id into `status_json` via a new `PanelReads::running_job() -> Option<String>`), `unit:<name>` when a service is `active|activating`, `other:<holder line>` when the lock holder line begins `recovery-os VM session` and neither of the former; `since_epoch` from `exec_main_start_epoch` or the holder's history? (`None` for `other`), `domain_state` from `reads.domain_state(label)`, `attended = Some(false)` for a unit, `None` for other.

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn the_service_unit_runs_the_script_unattended_waits_for_the_lock_and_never_lands_failed() {
    let s = render_service(&["system-recovery-A-2tb".into()], None, Path::new("/usr/lib/das-backup/recovery-os-vm.sh"));
    assert!(s.contains("ExecStart=/usr/lib/das-backup/recovery-os-vm.sh session system-recovery-A-2tb --unattended --wait-lock 180\n"), "{s}");
    assert!(s.contains("\nType=oneshot\n") && s.contains("\nSuccessExitStatus=1 3 4 5 6 7\n"), "{s}");
    assert!(!s.contains("Restart="), "sentinel must never see this unit failed, and systemd must never restart it");
    assert!(s.contains("After=network-online.target libvirtd.service\n") && s.contains("Wants=network-online.target\n"));
    let both = render_service(&["system-recovery-A-2tb".into(), "system-recovery-B-2tb".into()], Some("parallel"), Path::new("/x.sh"));
    assert!(both.contains("ExecStart=/x.sh session system-recovery-A-2tb system-recovery-B-2tb --mode parallel --unattended --wait-lock 180\n"));
}

#[test]
fn the_timer_is_one_shot_local_time_and_not_persistent() {
    let t = render_timer(&["system-recovery-A-2tb".into()], 1791500000); // 2026-10-08 17:53:20 CDT
    assert!(t.contains("OnCalendar=2026-10-08 17:53:20\n"), "{t}");
    assert!(t.contains("Persistent=false\n") && t.contains("Unit=das-recovery-os-update-system-recovery-A-2tb.service\n") && t.contains("WantedBy=timers.target\n"));
}

#[test]
fn a_timer_whose_time_passed_without_a_trigger_reads_missed() {
    let timer = UnitFacts { exists: true, active_state: "active".into(), next_elapse_epoch: None, last_trigger_epoch: None, on_calendar: Some("2026-10-08 03:00:00".into()), ..Default::default() };
    let service = UnitFacts { exists: true, active_state: "inactive".into(), ..Default::default() };
    let s = schedule_state(&timer, &service, None, 1791500000);
    assert_eq!(s.state, "missed"); assert!(s.detail.contains("2026-10-08 03:00:00"));
}

#[test]
fn a_fired_schedule_with_no_history_line_reads_the_units_result() {
    let timer = UnitFacts { exists: true, active_state: "active".into(), last_trigger_epoch: Some(1791490000), ..Default::default() };
    let service = UnitFacts { exists: true, active_state: "inactive".into(), result: Some("success".into()), exec_main_status: Some(1), last_journal: vec!["refuse: the boot record says btrbk will run".into()], ..Default::default() };
    let s = schedule_state(&timer, &service, None, 1791500000);
    assert_eq!(s.state, "fired"); assert!(s.detail.starts_with("refused:"), "{}", s.detail); assert!(s.detail.contains("will run"));
    let newer = serde_json::json!({"start": 1791490100, "outcome": "clean", "exit": 0});
    let s = schedule_state(&timer, &service, Some(&newer), 1791500000);
    assert_eq!(s.detail, "clean (exit 0)");
    let older = serde_json::json!({"start": 1791000000, "outcome": "clean", "exit": 0});
    assert!(schedule_state(&timer, &service, Some(&older), 1791500000).detail.starts_with("refused:"), "an older history line is not this firing's");
    let running = UnitFacts { exists: true, active_state: "activating".into(), exec_main_start_epoch: Some(1791499000), ..Default::default() };
    assert_eq!(schedule_state(&timer, &running, None, 1791500000).state, "running");
    let pending = UnitFacts { exists: true, active_state: "active".into(), next_elapse_epoch: Some(1791600000), ..Default::default() };
    assert_eq!(schedule_state(&pending, &service, None, 1791500000).state, "pending");
}

#[test]
fn validate_schedule_refuses_the_past_a_will_record_a_mode_for_one_drive_and_a_drive_already_scheduled_the_other_way() { /* six asserts in the shape of Task 4's validate test, using panel's fixtures; the "other way" case passes an `existing: &[String]` of unit bases — add that parameter */ }

#[test]
fn uninstall_removes_schedule_units_it_finds() { /* in installer.rs tests: a tempdir /etc/systemd/system with das-recovery-os-update-both.{service,timer} and an unrelated file; uninstall_with on a scripted UnitRunner must call disable --now on the timer and remove both files, leave the other */ }
```

- [ ] **Step 2: Run; fail.** **Step 3: Implement.** Renderer text (service):

```
[Unit]
Description=Scheduled update of the recovery drive OS: <labels>
Documentation=man:btrdasd(1)
Wants=network-online.target
After=network-online.target libvirtd.service
# Written by btrdasd-helper (RecoveryOsScheduleSet); removed when cleared or by uninstall.

[Service]
Type=oneshot
# Every outcome the driver reports (1 refused, 3/4 kept, 5 warnings, 6 guard, 7 stopped) travels by
# its history, summary and journal. None may leave this unit failed: cachyos-sentinel restarts
# failed units, and a restarted VM session is what must never happen unasked.
SuccessExitStatus=1 3 4 5 6 7
ExecStart=<script> session <labels> [--mode m] --unattended --wait-lock 180
```

Timer: `[Timer]\nOnCalendar=<local YYYY-MM-DD HH:MM:SS>\nPersistent=false\nUnit=<base>.service\n[Install]\nWantedBy=timers.target`. Local time: use `caldate` if it has a local formatter; otherwise `libc::localtime_r` via a tiny helper in `caldate.rs` (no chrono — check `Cargo.toml`; do not add a dependency). The helper's `RecoveryOsScheduleSet`: validate; `at_epoch == 0` → disable --now the timer (ignore "not loaded"), remove both files, daemon-reload; else write both files with `fsutil::write_atomic_mode(.., 0o644)`, `systemctl daemon-reload`, `systemctl enable --now <base>.timer`; every `systemctl` failure is `fdo::Error::Failed` with its stderr; on a failed enable, remove the files again. `SystemPanelReads::unit`: `systemctl show -p ActiveState,NextElapseUSecRealtime,LastTriggerUSec,Result,ExecMainStatus,ExecMainStartTimestampMonotonic?` — use `--timestamp=unix` (systemd ≥ 251) so `NextElapseUSecRealtime=@1791600000`; parse `@<secs>`; `journalctl -u <unit> -n 5 -o cat --no-pager`. Installer uninstall: glob the prefix in `/etc/systemd/system` through the existing path parameterisation of `uninstall_with` (it takes a manifest path; add a `unit_dir: &Path` parameter or read the existing one it uses for the generated units — read the function first).

- [ ] **Step 4: green.** **Step 5:** counter-test: drop `Persistent=false` from the renderer → timer test RED; restore. **Step 6: Commit** `recovery-OS schedules: generated unit pairs, RecoveryOsScheduleSet, status reads them; uninstall removes them`.

---

### Task 7: docs, rules, changelog, and the whole-branch checks

**Files:**
- Modify: `docs/ARCHITECTURE.md` (helper line 86 and 655: methods 20 → 25, polkit actions 7 → 8, signals 3; the `recovery_os` module row gains `panel`, `session`), `docs/DISASTER-RECOVERY-GUIDE.md` §"In the recovery-os-updater VM" (a subsection "Scheduled sessions": the unit names, `--wait-lock 180`, `Persistent=false` and what `missed` means, how to clear one by hand: `systemctl disable --now das-recovery-os-update-<label>.timer && rm /etc/systemd/system/das-recovery-os-update-<label>.{timer,service} && systemctl daemon-reload`), `.claude/rules/backup.md` §Sentinel Interaction (one bullet: `das-recovery-os-update-*.service` carries `SuccessExitStatus=1 3 4 5 6 7`, load-bearing; the outcome is in the history), `CHANGELOG.md` `## [Unreleased]` → `### Added`: four entries (the five helper methods and the action; the schedules; `--wait-lock`; the status document), `scripts/recovery-os-vm.sh` header already done in Task 1.
- Also `docs/btrdasd.1`: if it lists the helper's methods or polkit actions anywhere (grep `org.dasbackup`), update the count; else nothing.

- [ ] **Step 1:** write the docs; `codespell docs CHANGELOG.md .claude/rules/backup.md` clean; `markdownlint` if configured in the repo (check `.markdownlint*`).
- [ ] **Step 2: Whole-branch verification, every line pasted into the final report:**
  - `cmake -S . -B /tmp/das-8249-s2-build -DCMAKE_BUILD_TYPE=Release -DBUILD_TESTING=ON && cmake --build /tmp/das-8249-s2-build` — the build's last line
  - `cd indexer && CARGO_TARGET_DIR=/tmp/das-8249-s2-target cargo test` and `… --features dbus` — each `test result:` line
  - `ctest --test-dir /tmp/das-8249-s2-build -DREQUIRE_ALL_SHELL_CASES=ON --output-on-failure` — the `tests passed` line (shell suite included)
  - `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `… --features dbus`, `shellcheck scripts/*.sh tests/*.sh`, `shfmt -d -i 4 -ci scripts/recovery-os-vm.sh`
  - the mutation gate on a COPY (Global Constraints) — `mutants OK` and the caught/missed/unviable counters
  - `~/.claude/bin/scrub-promo check origin/main..HEAD` → `CLEAN`
- [ ] **Step 3: Commit** `docs: recovery-OS helper methods, schedules and --wait-lock (8249 stage 2)`.

---

## Not in this plan (stage 3 and later)

- The GUI panel (`gui/src/recoverypanel.{h,cpp}`, sidebar entry, `DBusClient` wrappers, the console viewer launch `remote-viewer vnc+unix://<path>` or equivalent) — stage 3.
- Install on the host, and the real-VM proofs still owed from stage 1: `console-socket`, a two-drive run, the attended `will` banner — stage 3, on the loop-hatch copy of `test-vm-cachyos`.
- `recovery-os-vm.sh status --json` — not needed: status is assembled from the record, the history, the units and `virsh domstate`.
