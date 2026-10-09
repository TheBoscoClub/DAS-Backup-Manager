# Recovery Drives in the GUI (8249 stage 3) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `btrdasd-gui` gains a **Recovery drives** section that shows each recovery drive's
CachyOS status from the helper's stage 2 document, starts attended or unattended update sessions
(now, or scheduled per drive or for both), opens the attended console, and ends a session a crashed
driver left behind — then stages 1–3 are installed on the host and the three real-VM proofs owed
since stage 1 are taken from the GUI.

**Architecture:** A pure model (`gui/src/recoverystatus.{h,cpp}`, QtCore only) parses the status
document into plain structs and derives, per button, `{enabled, why}` from the document plus what
only the window knows (`GuiFacts`). A thin widget (`gui/src/recoverypanel.{h,cpp}`) binds. One
checked-in fixture is the contract between `panel.rs`'s `status_json` (a Rust test writes and
compares it) and the C++ parser (the smoke test reads it). Five `DBusClient` wrappers; no helper
method changes. Two script/library defects the panel would otherwise display are fixed first.

**Tech Stack:** C++20 / Qt 6.6+ / KF6 (`gui/`), QTest (`gui/tests/smoketest.cpp`, offscreen), Rust
2024 (`indexer/src/recovery_os/panel.rs`), bash (`scripts/recovery-os-vm.sh`,
`tests/test_recovery_os_vm.sh`), CMake/ctest, libvirt + virt-viewer.

**Spec:** `docs/superpowers/specs/2026-10-09-recovery-os-gui-stage3-design.md` (approved
2026-10-09). Read it whole before Task 1; its §4 table is the rules Task 4 pins and §7 is Task 8.
The stage 2 plan (`docs/superpowers/plans/2026-10-08-recovery-os-helper-stage2.md`) describes
the document and the methods this plan consumes.

## Global Constraints

- Worktree, never the main checkout: `git worktree add /tmp/das-8249-s3 -b 8249-stage3 main`. Build only through CMake into tmpfs: `cmake -S . -B /tmp/das-8249-s3-build -DCMAKE_BUILD_TYPE=Release -DBUILD_TESTING=ON && cmake --build /tmp/das-8249-s3-build`. Never a bare `cargo build`.
- Rust tests: `cd indexer && CARGO_TARGET_DIR=/tmp/das-8249-s3-target cargo test` and again with `--features dbus`. Shell suite: `bash tests/test_recovery_os_vm.sh` (~5 min). GUI suite: `ctest --test-dir /tmp/das-8249-s3-build -R gui-smoketest --output-on-failure` (ctest sets `QT_QPA_PLATFORM=offscreen` and a dead system-bus address; never run `bin/gui-smoketest` by hand without both).
- Before every commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` (both feature sets), `shellcheck scripts/*.sh tests/*.sh`, `shfmt -d -i 4 -ci scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh`, `codespell docs CHANGELOG.md gui/src gui/tests`. C++ builds with `-Wall -Wextra -Wpedantic -Werror` (already set in `gui/CMakeLists.txt`); clang-format is not used in this tree — match the surrounding style (4 spaces, `m_` members, `Q_EMIT`, `QStringLiteral`).
- **Every interactive element carries a tooltip** (`setToolTip`, the project's UI rule). The tooltip of a disabled button says why; of an enabled one, what it does.
- Fail-silent law (`.claude/rules/fail-silent.md`): a missing reading renders as "unknown" or the error's words, never as 0, "no", or an enabled button; a document part that cannot be read disables what depends on it.
- The GUI **never passes `accept_boot_record_risk = true`** to `RecoveryOsSession` (spec §3.3).
- No helper method, signal or document key changes in this plan (spec §10); `panel.rs` changes only for bd `6obo` (Task 2) and the fixture test (Task 3).
- Install (`cmake --install`, `btrdasd setup --upgrade`) happens **only in Task 8**, from the main checkout after merge, and only when no `das-*` unit is active and `/run/das-backup.lock` is free (`.claude/rules/backup.md`, the busy check verbatim in Task 8).
- Script version `2.1.0` → `2.1.1` (header `# Version:` and `# Date:`) in Task 1. No project version bump (`## [Unreleased]`; `/git-release` bumps). `.staged-release` exists: commits may be pushed, **no tag, no `gh release`**.
- Mutation gate on the Rust diff (copy of the tree, `indexer/`): `git diff --relative origin/main -- . > mutants.diff && env -u CARGO_TARGET_DIR cargo mutants --in-place --no-shuffle --in-diff mutants.diff && env -u CARGO_TARGET_DIR python3 ../.github/scripts/mutants-gate.py` must print `mutants OK`. Shim mail first (a `mailx`/`s-nail`/`sendmail` stub first on PATH).
- Commits: signed, heredoc message, no trailer, no AI attribution of any kind. `~/.claude/bin/scrub-promo check origin/main..HEAD` must print CLEAN before any push.
- Real recovery drives are never lent during this plan's proofs (Task 8 uses loop-hatch copies of `test-vm-cachyos`, as stage 1's proof did).

## Review Focus

1. **A document part the helper could not read** (`session_error`, `schedule_error`, `record_error` set, the value `null`). Expected: that part shows the error's words in the warning colour, and every button depending on it is disabled with the error as its tooltip — never an enabled Upgrade beside "session: unknown". Pinned in Task 4 (`anErrorInAPartDisablesWhatDependsOnIt`).
2. **A job session of this window on a `will` drive, then the window restarts.** Expected: the session shows as `job:<id>` with `attended` unknown; Open console is disabled with "cannot tell whether this session is attended" (the GUI only knows attendedness of jobs it started); End session is disabled ("the session is a running job — use Cancel"). Pinned in Task 4 (`consoleNeedsAnAttendedSessionThisWindowKnows`).
3. **The chosen time is in the past, or less than 2 minutes ahead, or Now is ticked.** Expected: Schedule disabled with "pick a time at least 2 minutes ahead" / "untick Now to schedule"; the helper's own `SCHEDULE_MIN_LEAD` refusal is never reached by a well-formed click. Pinned in Task 4 (`scheduleNeedsATimeTwoMinutesAhead`).
4. **A parse failure** (not JSON, `schema` 2, a drive missing `unattended`). Expected: one line naming the problem replaces the cards, every button disabled, the refresh timer keeps running so a fixed helper recovers the panel. Pinned in Task 3 (`parseRefusesAnotherSchemaAndAMissingKey`) and Task 6 (`panelShowsTheParseErrorAndDisablesEverything`).
5. **`JobFinished(false, "warnings: …")`** (the script's exit 5). Expected: the panel's status line says *needs a look* with the summary, not *failed*; `JobFinished(false, anything else)` says *failed*. Pinned in Task 6 (`aWarnedSessionNeedsALookAFailedOneFailed`).

## Decisions taken in this plan (for the operator to confirm at review)

- **Console on a job session needs this window's knowledge.** The document's `session.attended` is `null` for a `job:` session (the helper does not record the request's mode in the document). Open console is enabled for `by == "job:" + ownJobId` only when this window started that job attended, or for any session whose `attended` is `true` — never on `null`. Spec §4 said `session.attended == true`; this is that rule made reachable for the one case where the field cannot be `true`.
- **The Progress dock's Cancel tooltip gains no parallel caveat**: Task 1 fixes bd `c8lf` before the panel exists, so the caveat the spec allowed "while the defect stands" never ships. The helper's doc comment and the CHANGELOG line that describe the defect are corrected in Task 1.
- **bd `epmw` (orphan schedule units) is deferred** — "read-only if cheap" would need a document key, and this plan changes no helper key. It stays a tracked follow-up, named in Task 7's closing report.
- **The fixture is the Rust output, byte for byte.** The Rust contract test regenerates `gui/tests/fixtures/recovery-status.json` when `RECOVERY_FIXTURE_WRITE=1` is set and otherwise asserts the committed file equals `status_json`'s pretty-printed output for a fixed scripted scenario. Stronger than a key-set comparison, and the C++ tests assert on that scenario's known values.
- **Refresh cadence 30 s while shown**, stopped on leaving the section; a refresh in flight is not re-issued (one `m_refreshInFlight` flag cleared by the result or the error).

## File Structure

| File | Responsibility |
|---|---|
| `scripts/recovery-os-vm.sh` (modify) | parallel pair children start with the default INT disposition (bd `c8lf`); version 2.1.1 |
| `tests/test_recovery_os_vm.sh` (modify) | the parallel-SIGINT case |
| `indexer/src/bin/btrdasd-helper.rs` (modify) | the `recovery_os_session` doc comment no longer describes `c8lf` |
| `indexer/src/recovery_os/panel.rs` (modify) | bd `6obo`: untruncated histories for the pair, earliest line after the trigger; the fixture contract test |
| `gui/tests/fixtures/recovery-status.json` (create) | the one contract document, written by the Rust test |
| `gui/src/recoverystatus.h/.cpp` (create) | `RecoveryDocument::parse`, `deriveActions`, `derivePairActions`, the formatting helpers |
| `gui/src/recoverypanel.h/.cpp` (create) | the widget |
| `gui/src/dbusclient.h/.cpp` (modify) | five wrappers, `consoleCommand`, the session-end timeout |
| `gui/src/sidebar.h/.cpp`, `gui/src/mainwindow.h/.cpp` (modify) | the section and page 5 |
| `gui/CMakeLists.txt` (modify) | the two new sources in both targets; the fixture path definition for the test |
| `gui/tests/smoketest.cpp` (modify) | parse, rules, formatting, construction, timeout, console command, needs-a-look |
| `packaging/debian/control`, `packaging/fedora/das-backup-manager.spec`, `packaging/flatpak/org.theboscoclub.btrdasd-gui.yml`, `packaging/snap/snapcraft.yaml` (modify) | virt-viewer recommended; the sandbox note (`packaging/arch/PKGBUILD` already lists it) |
| `docs/ARCHITECTURE.md`, `docs/DISASTER-RECOVERY-GUIDE.md`, `CHANGELOG.md` (modify) | the components, "From the GUI", the entries |

---

### Task 1: bd `c8lf` — a parallel pair's drive sessions must receive SIGINT

**Files:**
- Modify: `scripts/recovery-os-vm.sh` (`cmd_session_pair`, the parallel branch ~line 4414; header lines 3–4)
- Modify: `tests/test_recovery_os_vm.sh` (after the "both, parallel" cases, ~line 4841)
- Modify: `indexer/src/bin/btrdasd-helper.rs` (the doc comment above `async fn recovery_os_session`, ~line 1898)
- Modify: `CHANGELOG.md` (the `RecoveryOsSession` entry's parenthesis under `### Added`)

**Interfaces:**
- Consumes: the script's `run_interrupted` test harness (`tests/test_recovery_os_vm.sh` ~line 1197: starts the driver in its own process group, `kill -INT -- -$dpid` once the driver prints `waiting for the recovery OS to power off`, or once `WAIT_FILE` exists).
- Produces: a parallel pair whose two children exit 3 on a SIGINT to the group; `DRIVE <label> 3` for both; the run's own exit from `pair_status` (3).

The mechanism (bash, "Signals" in `man bash`): a command started with `&` by a non-interactive shell begins with SIGINT and SIGQUIT **ignored**, and no `trap` in the child — nor in a `bash -c` wrapper, which inherits the same ignored-at-entry state — can undo that. The parent's `trap ':' INT` is irrelevant; the `&` is. What resets the disposition before the exec is `env --default-signal=INT` (coreutils ≥ 8.32, present on CachyOS and Debian 11+); the suite already uses exactly that for itself (line 29). Use it.

- [ ] **Step 1: Write the failing suite case** (append after the `both, parallel: no egress rule left` check, before the `--wait-lock` loop):

```bash
# bd c8lf: a child started with `&` begins with SIGINT ignored, and nothing
# in the child can undo that; the parent must start it with the default
# disposition. Before the fix a Ctrl-C (or the helper's JobCancel) stopped
# nothing in parallel mode: both drives ran to the end.
unattended_fixture
WAIT_FILE="$S/domB/events" run_interrupted session A B --unattended --mode parallel
check "both, parallel, SIGINT: the run exits 3" "$RC" "3"
has "both, parallel, SIGINT: A stopped" "$OUT" "DRIVE system-recovery-A-2tb 3"
has "both, parallel, SIGINT: B stopped" "$OUT" "DRIVE system-recovery-B-2tb 3"
check "both, parallel, SIGINT: no drive ran its upgrade step" "$(grep -c upgrade "$S/stages.log" || :)" "0"
check "both, parallel, SIGINT: never destroyed" "$(file "$S/forbidden")" ""
unset WAIT_FILE
```

Read `unattended_fixture` (grep for it) and `$S/stages.log` first: the stub's steps append their names there, and `domB/events` is written when B's domain starts — the signal must arrive after both children have started but before their upgrade steps. If the fixture's steps are too fast for that window, slow the stub's `egress` step with `echo 2 >"$S/stage.egress.sleep"` if such a seam exists, else with the `POLL`/`GRACE` env the harness takes (`POLL=0.5` lengthens every wait). Prove the window: run the case once with `set -x` on the harness and confirm the signal lands with both `virsh start` lines present and no `upgrade` line.

- [ ] **Step 2: Run the suite's new section to see it fail**

Run: `bash tests/test_recovery_os_vm.sh 2>&1 | grep -A2 'parallel, SIGINT'`
Expected: `FAIL both, parallel, SIGINT: the run exits 3` (got 0) and `FAIL … A stopped` — both drives ran to the end.

- [ ] **Step 3: Fix the script** — in the parallel branch replace

```bash
            DAS_RECOVERY_VM_LOCK_FD=$LOCK_FD DAS_RECOVERY_VM_EGRESS_HELD=1 DAS_RECOVERY_VM_MODE=parallel \
                bash "$SELF" session "$l" "${flags[@]}" &
```

with

```bash
            # Started with `&`, a child begins with SIGINT ignored and cannot
            # take that back; env resets it before the exec, so Ctrl-C and
            # the helper's JobCancel reach both drives' sessions (bd c8lf).
            DAS_RECOVERY_VM_LOCK_FD=$LOCK_FD DAS_RECOVERY_VM_EGRESS_HELD=1 DAS_RECOVERY_VM_MODE=parallel \
                env --default-signal=INT bash "$SELF" session "$l" "${flags[@]}" &
```

and correct the comment above `trap ':' INT TERM HUP`:

```bash
    # An interrupt reaches the drives' sessions themselves (they are in this
    # process group, and the parallel branch starts them with SIGINT
    # deliverable); this one waits for them to end.
```

Bump the header: `# Version: 2.1.1`, `# Date: 2026-10-09`.

- [ ] **Step 4: Run the new section, then the whole suite**

Run: `bash tests/test_recovery_os_vm.sh 2>&1 | tail -3`
Expected: the new five checks PASS; the final line reports 0 failures (the count grows by 5 from 1575).

- [ ] **Step 5: Counter-test the fix** — revert Step 3's `env --default-signal=INT` only (keep the case), run the section, see the two "stopped" checks RED again, restore. Record both results in the commit message's body.

- [ ] **Step 6: Correct the helper's doc comment and the CHANGELOG** — in `btrdasd-helper.rs` replace the sentence from `In \`--mode parallel\` that does not` through `parallel is not refused for it.` with: `In --mode parallel the signal reaches both drives' sessions (the script starts them with SIGINT deliverable; bd DAS-Backup-Manager-c8lf).` In `CHANGELOG.md`'s `RecoveryOsSession` parenthesis delete `— in \`--mode parallel\` that does not yet stop the drive sessions, script defect bd \`DAS-Backup-Manager-c8lf\``; add under `### Fixed`:

```markdown
- **A cancelled parallel two-drive run now stops both drives' sessions** (bd `DAS-Backup-Manager-c8lf`) — a child started with `&` begins with SIGINT ignored and cannot take that back, so Ctrl-C and the helper's `JobCancel` stopped nothing in `--mode parallel`; the run starts each drive's session through `env --default-signal=INT`, and a SIGINT to the group ends both with exit status 3, the lock kept and the recovery OSes left running for `session-end`
```

- [ ] **Step 7: Lint and commit**

```bash
shellcheck scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh && shfmt -d -i 4 -ci scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh
cd indexer && CARGO_TARGET_DIR=/tmp/das-8249-s3-target cargo test --features dbus session_busy 2>&1 | tail -3; cd ..
git add scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh indexer/src/bin/btrdasd-helper.rs CHANGELOG.md
git commit -S -m "$(cat <<'EOF'
recovery-os-vm.sh 2.1.1: a parallel pair's sessions receive SIGINT (c8lf)

A child started with & begins with SIGINT ignored and cannot take that
back, so Ctrl-C and JobCancel stopped nothing in --mode parallel. The
run starts each drive's session through env --default-signal=INT.
Suite: parallel pair + SIGINT to the group -> both DRIVE lines 3, no
upgrade step; with the env prefix reverted the two "stopped" checks
went red and came back with it.
EOF
)"
```

---

### Task 2: bd `6obo` — the pair's firing is searched in the whole histories, earliest after the trigger

**Files:**
- Modify: `indexer/src/recovery_os/panel.rs` (`DriveStatus` ~line 201, `drive_status` ~line 335, `status_json` ~line 390, `line_in` ~line 538)
- Test: `panel.rs` `mod tests`

**Interfaces:**
- Produces: `DriveStatus` gains `pub full_history: Vec<Value>` (every line, newest last; not serialised — `to_json` is hand-written and does not list it); `status_json` builds `pair_lines` from `full_history`; `line_in` returns the **earliest** line at or after `from` (and at or before `to` when given).

- [ ] **Step 1: Write the failing tests** (in `mod tests`, next to `status_fills_schedules_and_sessions_from_units_jobs_and_the_lock` ~line 2314; reuse its helpers for a fired `both` timer — copy how that test builds `UnitFacts` for a timer with `last_trigger_epoch` and a service with `exec_main_start_epoch`/`exec_main_exit_epoch`):

```rust
#[test]
fn a_pair_firing_older_than_the_display_cut_is_still_found() {
    // 25 later lines per drive push the pair's firing out of the 20-line
    // display cut; the pair must still read its outcome (bd 6obo 1).
    let cfg = two_mirrors_and_a_primary();
    let trigger = 1_791_000_000;
    let mut a_lines = vec![json!({"label":"system-recovery-A-2tb","start":trigger+5,"end":trigger+900,"mode":"sequential","unattended":true,"outcome":"clean","exit":0})];
    let mut b_lines = vec![json!({"label":"system-recovery-B-2tb","start":trigger+905,"end":trigger+1800,"mode":"sequential","unattended":true,"outcome":"clean","exit":0})];
    for i in 0..25 {
        let s = trigger + 10_000 + i * 1000;
        a_lines.push(json!({"label":"system-recovery-A-2tb","start":s,"end":s+10,"mode":null,"unattended":false,"outcome":"clean","exit":0}));
        b_lines.push(json!({"label":"system-recovery-B-2tb","start":s,"end":s+10,"mode":null,"unattended":false,"outcome":"clean","exit":0}));
    }
    let reads = Scripted {
        history: [("system-recovery-A-2tb", Ok(a_lines)), ("system-recovery-B-2tb", Ok(b_lines))].into(),
        units: fired_pair_units(trigger, trigger + 2, trigger + 1801), // timer triggered; service ran start..exit
        ..Default::default()
    };
    let j = status_json(&cfg, &Ok(None), &host(), "2026-10-09", trigger + 50_000, &reads);
    assert_eq!(j["pair"]["schedule"]["state"], "fired");
    assert!(j["pair"]["schedule"]["detail"].as_str().unwrap().starts_with("clean (exit 0)"),
        "{}", j["pair"]["schedule"]["detail"]);
    assert_eq!(j["drives"][0]["history"].as_array().unwrap().len(), 20, "display cut unchanged");
}

#[test]
fn a_firing_without_its_own_exec_readings_takes_the_earliest_line_after_the_trigger() {
    // Exec readings older than the trigger bound nothing; the window is
    // open-ended, and the line of THIS firing is the earliest after it,
    // not a later Now session's (bd 6obo 2).
    let trigger = 1_791_000_000;
    let lines = [
        json!({"start":trigger+5,"outcome":"refused","exit":1,"unattended":true}),
        json!({"start":trigger+9000,"outcome":"clean","exit":0,"unattended":false}),
    ];
    let refs: Vec<&Value> = lines.iter().collect();
    let found = line_in(&refs, trigger, None, false).unwrap();
    assert_eq!(found["outcome"], "refused");
    // Bounded: the earliest inside the window is still the right one.
    let found = line_in(&refs, trigger, Some(trigger + 10_000), false).unwrap();
    assert_eq!(found["outcome"], "refused");
    // Nothing at or after `from`: none.
    assert!(line_in(&refs, trigger + 20_000, None, false).is_none());
}
```

Write `fired_pair_units(trigger, start, exit) -> HashMap<String, Result<UnitFacts, String>>` in the test module: `das-recovery-os-update-both.timer` → `UnitFacts { exists: true, active_state: "active".into(), last_trigger_epoch: Some(trigger), on_calendar: Some("2026-10-09 03:00:00".into()), ..Default::default() }`, `…-both.service` → `UnitFacts { exists: true, active_state: "inactive".into(), exec_main_start_epoch: Some(start), exec_main_exit_epoch: Some(exit), exec_main_status: Some(0), result: Some("success".into()), exec_start: Some("… --mode sequential …".into()), ..Default::default() }` — read how the existing `status_fills_schedules…` test spells `exec_start` so `mode_of` parses it.

- [ ] **Step 2: Run them to see them fail**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-8249-s3-target cargo test 6obo_ 2>/dev/null; cargo test a_pair_firing_older a_firing_without -- --nocapture 2>&1 | tail -15`
Expected: the first fails (`detail` begins `refused:` — the pair's line was cut), the second fails on `assert_eq!(found["outcome"], "refused")` (got `clean`).

- [ ] **Step 3: Implement**

In `DriveStatus` add `pub full_history: Vec<Value>,` (doc: `/// Every history line, newest last; the pair's firing is searched here, not in the display cut.`). In `drive_status`, before `let skip = …`, keep a clone: `let full_history = history.clone();` and set `full_history,` in the struct literal. In `status_json` change `let pair_lines: Vec<&Value> = drives.iter().flat_map(|d| &d.history).collect();` to `flat_map(|d| &d.full_history)`. In `line_in` change `.max_by_key(` to `.min_by_key(` and the doc comment on `schedule_state` from "the latest history line that started inside" to "the earliest history line that started inside".

- [ ] **Step 4: Run the panel tests**

Run: `cd indexer && CARGO_TARGET_DIR=/tmp/das-8249-s3-target cargo test recovery_os::panel 2>&1 | tail -3`
Expected: all pass. If an existing test asserted the latest line, read why — a test pinning "latest" for a bounded window where two sessions sit inside one service run is pinning the wrong one; change its expectation and say so in the commit.

- [ ] **Step 5: Commit**

```bash
cd indexer && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cd ..
git add indexer/src/recovery_os/panel.rs
git commit -S -m "$(cat <<'EOF'
panel: the pair's firing is searched in the whole histories (6obo)

pair_lines came from each drive's 20-line display cut, so a pair
firing older than 20 sessions read refused or missed; and with no exec
readings of its own a firing took the latest line after the trigger,
a later Now session's. The pair reads the untruncated histories and
line_in takes the earliest line at or after the window's start.
EOF
)"
```

---

### Task 3: the contract fixture and `RecoveryDocument::parse`

**Files:**
- Modify: `indexer/src/recovery_os/panel.rs` (`mod tests`: the fixture test)
- Create: `gui/tests/fixtures/recovery-status.json` (written by that test)
- Create: `gui/src/recoverystatus.h`, `gui/src/recoverystatus.cpp`
- Modify: `gui/CMakeLists.txt` (sources; a compile definition with the fixture path for the test)
- Modify: `gui/tests/smoketest.cpp`

**Interfaces:**
- Produces (`recoverystatus.h`):

```cpp
#pragma once
#include <QByteArray>
#include <QJsonObject>
#include <QList>
#include <QString>
#include <QStringList>
#include <optional>

// The stage 2 status document (RecoveryOsStatus, schema 1) as plain structs.
// Every *_error is an optional: present means that part could not be read
// and its value is null. See the spec's §3.1.

struct GuestAgentView { QString state; std::optional<bool> installed; std::optional<bool> enabled; QString why; };
struct RecordView {
    QString os; QString installed; QString lastFullUpgrade;
    QString kernel; QString hostKernel; QString btrfsProgs; QString hostBtrfsProgs; QString btrbk;
    bool packagesRead = false; GuestAgentView guestAgent;
};
struct AssessmentView { std::optional<qint64> ageDays; QString ageBasis; bool stale = false; QStringList reasons; QStringList warnings; };
struct ScheduleView { QString unit; std::optional<qint64> atEpoch; QString mode; QString state; QString detail; };
struct SessionView { QString by; std::optional<qint64> sinceEpoch; QString domainState; std::optional<bool> attended; };
struct DriveView {
    QString label; QString displayName; QStringList serials;
    std::optional<qint64> checkedEpoch;
    std::optional<QString> recordError; std::optional<RecordView> record;
    std::optional<AssessmentView> assessment; bool due = false; QString verdict; // "will"|"may"|"no"|""
    bool unattendedPossible = false; QString unattendedWhy;
    std::optional<qint64> cleanRuns; std::optional<QString> cleanRunsError;
    QList<QJsonObject> history; std::optional<QString> historyError;
    std::optional<ScheduleView> schedule; std::optional<QString> scheduleError;
    std::optional<SessionView> session; std::optional<QString> sessionError;
};
struct PairView {
    QString modeDefault;
    std::optional<ScheduleView> schedule; std::optional<QString> scheduleError;
    std::optional<SessionView> session; std::optional<QString> sessionError;
};
struct RecoveryDocument {
    int schema = 0; int maxAgeDays = 0; QString today; PairView pair; QList<DriveView> drives;
    // Null document on failure; `error` names the problem ("not JSON", "schema 2 is not 1",
    // "drives[1] lacks unattended").
    static std::optional<RecoveryDocument> parse(const QByteArray &json, QString *error);
};
```

- [ ] **Step 1: Write the Rust fixture test** (in `panel.rs` `mod tests`; it fixes the scenario Tasks 3–6 assert on):

```rust
/// The GUI's contract: gui/tests/fixtures/recovery-status.json IS this
/// output. Regenerate with RECOVERY_FIXTURE_WRITE=1; otherwise the committed
/// file must equal it byte for byte, so a key or wording change on either
/// side fails here and in gui-smoketest.
#[test]
fn the_gui_fixture_is_this_status_document() {
    let cfg = two_mirrors_and_a_primary();
    let state = Ok(Some(state_v(
        4,
        &[("system-recovery-A-2tb", stored_drive(agent_at_boot(), BootVerdict::May, Some(uuid_a())))],
    )));
    let mut units = HashMap::new();
    units.insert("das-recovery-os-update-system-recovery-A-2tb.timer".to_string(), Ok(UnitFacts {
        exists: true, active_state: "active".into(), next_elapse_epoch: Some(1_791_600_000),
        on_calendar: Some("2026-10-10 03:00:00".into()), ..Default::default() }));
    units.insert("das-recovery-os-update-system-recovery-A-2tb.service".to_string(), Ok(UnitFacts {
        exists: true, active_state: "inactive".into(),
        exec_start: Some("/usr/lib/das-backup/recovery-os-vm.sh session system-recovery-A-2tb --unattended --wait-lock 180".into()),
        ..Default::default() }));
    units.insert("das-recovery-os-update-system-recovery-B-2tb.timer".to_string(), Err("systemctl show failed: Connection refused".into()));
    let reads = Scripted {
        clean: [("system-recovery-A-2tb", Ok(2)), ("system-recovery-B-2tb", Err("history cannot be read: line 3 is not JSON".into()))].into(),
        history: [
            ("system-recovery-A-2tb", Ok(vec![
                json!({"label":"system-recovery-A-2tb","start":1_791_300_000,"end":1_791_300_310,"mode":null,"unattended":true,"outcome":"clean","exit":0,"overridden":false,"kernel":"7.2.9-1-cachyos","stopped_at":null}),
                json!({"label":"system-recovery-A-2tb","start":1_791_400_000,"end":1_791_400_290,"mode":null,"unattended":true,"outcome":"clean","exit":0,"overridden":false,"kernel":"7.2.9-1-cachyos","stopped_at":null}),
            ])),
            ("system-recovery-B-2tb", Err("history cannot be read: line 3 is not JSON".into())),
        ].into(),
        units,
        lock: Some("recovery-os VM session system-recovery-A-2tb pid 4242".into()),
        domain: Some("running".into()),
        job: Some("job-7".into()),
        ..Default::default()
    };
    let j = status_json(&cfg, &state, &host(), "2026-10-09", 1_791_500_000, &reads);
    let text = serde_json::to_string_pretty(&j).unwrap() + "\n";
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../gui/tests/fixtures/recovery-status.json");
    if std::env::var_os("RECOVERY_FIXTURE_WRITE").is_some() {
        std::fs::write(path, &text).unwrap();
    }
    let on_disk = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(on_disk, text, "the fixture is stale: RECOVERY_FIXTURE_WRITE=1 cargo test the_gui_fixture");
}
```

Read `host()` and `two_mirrors_and_a_primary()` in the test module first: `host()` must return fixed strings (a kernel and a btrfs-progs version), never the running host's, or the fixture differs per machine. If it reads the host, write a `fixed_host()` for this test. Likewise `today` is the literal above and `now` is the literal epoch; the record's `checked_epoch` comes from `stored_drive` — read it, and if it uses the clock, pass a fixed one.

- [ ] **Step 2: Generate the fixture, inspect it, run the test without the variable**

Run: `mkdir -p gui/tests/fixtures && cd indexer && RECOVERY_FIXTURE_WRITE=1 CARGO_TARGET_DIR=/tmp/das-8249-s3-target cargo test the_gui_fixture 2>&1 | tail -3 && cd .. && jq '.drives[] | {label, verdict, due, unattended, clean_runs, clean_runs_error, record_error, history_error, schedule: .schedule.state, schedule_error, session: .session.by, session_error}' gui/tests/fixtures/recovery-status.json && cd indexer && CARGO_TARGET_DIR=/tmp/das-8249-s3-target cargo test the_gui_fixture 2>&1 | tail -3`
Expected: drive A: `verdict "may"`, `due false`, `unattended.possible true`, `clean_runs 2`, `schedule "pending"`, `session "job:job-7"`, errors null. Drive B: `record_error "no record of system-recovery-B-2tb"`, `clean_runs_error` and `history_error` set, `schedule_error` beginning `das-recovery-os-update-system-recovery-B-2tb.timer cannot be read`, `session_error` set (its `…-both` or own unit read fails — if it is null, make `units` return `Err` for `das-recovery-os-update-both.service` too and regenerate), `session null`. Pair: `mode_default "sequential"`, `schedule null`, `schedule_error` set or null — note which, Task 4's tests use it. Second run: PASS. If any value differs between two generations (a clock leaked in), fix the leak before going on.

- [ ] **Step 3: Write the failing C++ parse tests** (new `private Q_SLOTS` block at the end of `GuiSmokeTest`, with `#include "../src/recoverystatus.h"` and `#include <QFile>`; the fixture path comes from the compile definition `RECOVERY_FIXTURE_PATH` added in Step 5):

```cpp
    static QByteArray fixture()
    {
        QFile f(QStringLiteral(RECOVERY_FIXTURE_PATH));
        if (!f.open(QIODevice::ReadOnly))
            return {};
        return f.readAll();
    }

    void parseReadsEveryPartOfTheFixture()
    {
        QString err;
        const auto doc = RecoveryDocument::parse(fixture(), &err);
        QVERIFY2(doc.has_value(), qPrintable(err));
        QCOMPARE(doc->schema, 1);
        QCOMPARE(doc->maxAgeDays, 60);
        QCOMPARE(doc->today, QStringLiteral("2026-10-09"));
        QCOMPARE(doc->pair.modeDefault, QStringLiteral("sequential"));
        QCOMPARE(doc->drives.size(), 2);

        const DriveView &a = doc->drives[0];
        QCOMPARE(a.label, QStringLiteral("system-recovery-A-2tb"));
        QVERIFY(!a.recordError.has_value());
        QVERIFY(a.record.has_value());
        QCOMPARE(a.record->kernel, QStringLiteral("7.2.9-1-cachyos"));
        QVERIFY(a.record->packagesRead);
        QCOMPARE(a.verdict, QStringLiteral("may"));
        QVERIFY(a.unattendedPossible);
        QVERIFY(a.unattendedWhy.isEmpty());
        QCOMPARE(a.cleanRuns, std::optional<qint64>(2));
        QCOMPARE(a.history.size(), 2);
        QVERIFY(a.schedule.has_value());
        QCOMPARE(a.schedule->state, QStringLiteral("pending"));
        QCOMPARE(a.schedule->atEpoch, std::optional<qint64>(1791600000));
        QVERIFY(a.session.has_value());
        QCOMPARE(a.session->by, QStringLiteral("job:job-7"));
        QCOMPARE(a.session->domainState, QStringLiteral("running"));
        QVERIFY(!a.session->attended.has_value()); // null in the document

        const DriveView &b = doc->drives[1];
        QVERIFY(b.recordError.has_value());
        QVERIFY(b.recordError->contains(QStringLiteral("no record")));
        QVERIFY(!b.record.has_value());
        QVERIFY(!b.cleanRuns.has_value());
        QVERIFY(b.cleanRunsError.has_value());
        QVERIFY(b.historyError.has_value());
        QVERIFY(b.scheduleError.has_value());
        QVERIFY(!b.schedule.has_value());
        QVERIFY(b.sessionError.has_value());
        QVERIFY(!b.session.has_value());
    }

    void parseRefusesAnotherSchemaAndAMissingKey()
    {
        QString err;
        QVERIFY(!RecoveryDocument::parse("not json", &err).has_value());
        QVERIFY(err.contains(QStringLiteral("not JSON")));

        QJsonDocument d = QJsonDocument::fromJson(fixture());
        QJsonObject o = d.object();
        o[QStringLiteral("schema")] = 2;
        QVERIFY(!RecoveryDocument::parse(QJsonDocument(o).toJson(), &err).has_value());
        QVERIFY2(err.contains(QStringLiteral("schema 2")), qPrintable(err));

        o = d.object();
        QJsonArray drives = o[QStringLiteral("drives")].toArray();
        QJsonObject drive = drives[1].toObject();
        drive.remove(QStringLiteral("unattended"));
        drives[1] = drive;
        o[QStringLiteral("drives")] = drives;
        QVERIFY(!RecoveryDocument::parse(QJsonDocument(o).toJson(), &err).has_value());
        QVERIFY2(err.contains(QStringLiteral("drives[1]")) && err.contains(QStringLiteral("unattended")), qPrintable(err));
    }
```

- [ ] **Step 4: Build to see them fail**

Run: `cmake --build /tmp/das-8249-s3-build --target gui-smoketest 2>&1 | grep -m3 error`
Expected: `recoverystatus.h: No such file or directory`.

- [ ] **Step 5: Implement `recoverystatus.{h,cpp}` and wire CMake**

`recoverystatus.h`: the interface block above. `recoverystatus.cpp`:

```cpp
#include "recoverystatus.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonParseError>
#include <QJsonValue>

namespace {

// A key that must exist (null allowed) — a document missing it is another
// helper's, and parsing on would show "unknown" for what is really "absent".
bool need(const QJsonObject &o, const char *key, const QString &where, QString *error)
{
    if (o.contains(QLatin1String(key)))
        return true;
    if (error)
        *error = QStringLiteral("%1 lacks %2").arg(where, QLatin1String(key));
    return false;
}

std::optional<QString> optString(const QJsonValue &v)
{
    return v.isString() ? std::optional<QString>(v.toString()) : std::nullopt;
}

std::optional<qint64> optInt(const QJsonValue &v)
{
    return v.isDouble() ? std::optional<qint64>(v.toInteger()) : std::nullopt;
}

std::optional<bool> optBool(const QJsonValue &v)
{
    return v.isBool() ? std::optional<bool>(v.toBool()) : std::nullopt;
}

QStringList strings(const QJsonValue &v)
{
    QStringList out;
    for (const QJsonValue &s : v.toArray())
        out << s.toString();
    return out;
}

std::optional<ScheduleView> schedule(const QJsonValue &v)
{
    if (!v.isObject())
        return std::nullopt;
    const QJsonObject o = v.toObject();
    return ScheduleView{o[QStringLiteral("unit")].toString(), optInt(o[QStringLiteral("at_epoch")]),
                        o[QStringLiteral("mode")].toString(), o[QStringLiteral("state")].toString(),
                        o[QStringLiteral("detail")].toString()};
}

std::optional<SessionView> session(const QJsonValue &v)
{
    if (!v.isObject())
        return std::nullopt;
    const QJsonObject o = v.toObject();
    return SessionView{o[QStringLiteral("by")].toString(), optInt(o[QStringLiteral("since_epoch")]),
                       o[QStringLiteral("domain_state")].toString(), optBool(o[QStringLiteral("attended")])};
}

std::optional<RecordView> record(const QJsonValue &v)
{
    if (!v.isObject())
        return std::nullopt;
    const QJsonObject o = v.toObject();
    RecordView r;
    r.os = o[QStringLiteral("os")].toString();
    r.installed = o[QStringLiteral("installed")].toString();
    r.lastFullUpgrade = o[QStringLiteral("last_full_upgrade")].toString();
    r.kernel = o[QStringLiteral("kernel")].toString();
    r.hostKernel = o[QStringLiteral("host_kernel")].toString();
    r.btrfsProgs = o[QStringLiteral("btrfs_progs")].toString();
    r.hostBtrfsProgs = o[QStringLiteral("host_btrfs_progs")].toString();
    r.btrbk = o[QStringLiteral("btrbk")].toString();
    r.packagesRead = o[QStringLiteral("packages_read")].toBool();
    const QJsonObject g = o[QStringLiteral("guest_agent")].toObject();
    r.guestAgent = GuestAgentView{g[QStringLiteral("state")].toString(), optBool(g[QStringLiteral("installed")]),
                                  optBool(g[QStringLiteral("enabled")]), g[QStringLiteral("why")].toString()};
    return r;
}

std::optional<AssessmentView> assessment(const QJsonValue &v)
{
    if (!v.isObject())
        return std::nullopt;
    const QJsonObject o = v.toObject();
    return AssessmentView{optInt(o[QStringLiteral("age_days")]), o[QStringLiteral("age_basis")].toString(),
                          o[QStringLiteral("stale")].toBool(), strings(o[QStringLiteral("reasons")]),
                          strings(o[QStringLiteral("warnings")])};
}

const char *const DriveKeys[] = {
    "label", "display_name", "serials", "checked_epoch", "record_error", "record", "assessment",
    "due", "verdict", "unattended", "clean_runs", "clean_runs_error", "history", "history_error",
    "schedule", "schedule_error", "session", "session_error",
};

std::optional<DriveView> drive(const QJsonValue &v, int index, QString *error)
{
    const QString where = QStringLiteral("drives[%1]").arg(index);
    if (!v.isObject()) {
        if (error)
            *error = where + QStringLiteral(" is not an object");
        return std::nullopt;
    }
    const QJsonObject o = v.toObject();
    for (const char *key : DriveKeys)
        if (!need(o, key, where, error))
            return std::nullopt;
    DriveView d;
    d.label = o[QStringLiteral("label")].toString();
    d.displayName = o[QStringLiteral("display_name")].toString();
    d.serials = strings(o[QStringLiteral("serials")]);
    d.checkedEpoch = optInt(o[QStringLiteral("checked_epoch")]);
    d.recordError = optString(o[QStringLiteral("record_error")]);
    d.record = record(o[QStringLiteral("record")]);
    d.assessment = assessment(o[QStringLiteral("assessment")]);
    d.due = o[QStringLiteral("due")].toBool();
    d.verdict = o[QStringLiteral("verdict")].toString();
    const QJsonObject u = o[QStringLiteral("unattended")].toObject();
    d.unattendedPossible = u[QStringLiteral("possible")].toBool();
    d.unattendedWhy = u[QStringLiteral("why")].toString();
    d.cleanRuns = optInt(o[QStringLiteral("clean_runs")]);
    d.cleanRunsError = optString(o[QStringLiteral("clean_runs_error")]);
    for (const QJsonValue &h : o[QStringLiteral("history")].toArray())
        d.history << h.toObject();
    d.historyError = optString(o[QStringLiteral("history_error")]);
    d.schedule = schedule(o[QStringLiteral("schedule")]);
    d.scheduleError = optString(o[QStringLiteral("schedule_error")]);
    d.session = session(o[QStringLiteral("session")]);
    d.sessionError = optString(o[QStringLiteral("session_error")]);
    return d;
}

} // namespace

std::optional<RecoveryDocument> RecoveryDocument::parse(const QByteArray &json, QString *error)
{
    QJsonParseError pe;
    const QJsonDocument doc = QJsonDocument::fromJson(json, &pe);
    if (doc.isNull() || !doc.isObject()) {
        if (error)
            *error = QStringLiteral("the status document is not JSON: %1").arg(pe.errorString());
        return std::nullopt;
    }
    const QJsonObject o = doc.object();
    // The schema first: another schema's document is refused by its number,
    // not by the first key this schema has and it lacks.
    if (!need(o, "schema", QStringLiteral("the status document"), error))
        return std::nullopt;
    RecoveryDocument d;
    d.schema = o[QStringLiteral("schema")].toInt();
    if (d.schema != 1) {
        if (error)
            *error = QStringLiteral("the status document is schema %1, this GUI reads schema 1").arg(d.schema);
        return std::nullopt;
    }
    for (const char *key : {"max_age_days", "today", "pair", "drives"})
        if (!need(o, key, QStringLiteral("the status document"), error))
            return std::nullopt;
    d.maxAgeDays = o[QStringLiteral("max_age_days")].toInt();
    d.today = o[QStringLiteral("today")].toString();
    const QJsonObject p = o[QStringLiteral("pair")].toObject();
    for (const char *key : {"mode_default", "schedule", "schedule_error", "session", "session_error"})
        if (!need(p, key, QStringLiteral("pair"), error))
            return std::nullopt;
    d.pair.modeDefault = p[QStringLiteral("mode_default")].toString();
    d.pair.schedule = schedule(p[QStringLiteral("schedule")]);
    d.pair.scheduleError = optString(p[QStringLiteral("schedule_error")]);
    d.pair.session = session(p[QStringLiteral("session")]);
    d.pair.sessionError = optString(p[QStringLiteral("session_error")]);
    const QJsonArray drives = o[QStringLiteral("drives")].toArray();
    for (int i = 0; i < drives.size(); ++i) {
        auto dv = drive(drives[i], i, error);
        if (!dv)
            return std::nullopt;
        d.drives << *dv;
    }
    return d;
}
```

CMake: add `src/recoverystatus.cpp` to both `btrdasd-gui`'s and `gui-smoketest`'s source lists; after `add_executable(gui-smoketest …)` add
`target_compile_definitions(gui-smoketest PRIVATE RECOVERY_FIXTURE_PATH="${CMAKE_CURRENT_SOURCE_DIR}/tests/fixtures/recovery-status.json")`.

- [ ] **Step 6: Run the GUI suite**

Run: `cmake --build /tmp/das-8249-s3-build && ctest --test-dir /tmp/das-8249-s3-build -R gui-smoketest --output-on-failure 2>&1 | tail -5`
Expected: `100% tests passed`, the two new cases listed PASS in the output above (`grep -c 'PASS   : GuiSmokeTest::parse' ` → 2).

- [ ] **Step 7: Counter-test** — in `parse`, change `d.schema != 1` to `d.schema != 2` and rebuild: `parseReadsEveryPartOfTheFixture` must FAIL ("schema 1, this GUI reads…"); restore. Delete `"unattended",` from `DriveKeys`: `parseRefusesAnotherSchemaAndAMissingKey` must FAIL; restore.

- [ ] **Step 8: Commit**

```bash
git add gui/tests/fixtures/recovery-status.json indexer/src/recovery_os/panel.rs gui/src/recoverystatus.h gui/src/recoverystatus.cpp gui/CMakeLists.txt gui/tests/smoketest.cpp
git commit -S -m "$(cat <<'EOF'
gui: RecoveryDocument parses the status document; the fixture is its contract

gui/tests/fixtures/recovery-status.json is written by a Rust test from
a fixed scripted scenario and must equal status_json's output byte for
byte; gui-smoketest parses the same file. A missing key or another
schema is a parse error naming it.
EOF
)"
```

---

### Task 4: the enablement rules — `deriveActions` and `derivePairActions`

**Files:**
- Modify: `gui/src/recoverystatus.h`, `gui/src/recoverystatus.cpp`
- Modify: `gui/tests/smoketest.cpp`

**Interfaces:**
- Produces (append to `recoverystatus.h`):

```cpp
// What only the window knows.
struct GuiFacts {
    QString ownJobId;              // this window's running session job, empty if none
    bool ownJobAttended = false;   // how this window started that job
    bool viewerInstalled = false;  // remote-viewer found on PATH
    qint64 nowEpoch = 0;
    bool chosenNow = true;         // the Now checkbox
    qint64 chosenEpoch = 0;        // the date-time picker
    bool unattended = false;       // the radio
};

struct Action { bool enabled = false; QString why; };

struct DriveActions {
    Action upgrade;        // for the chosen attended/unattended radio
    Action schedule;
    Action clearSchedule;
    Action console;
    Action endSession;
    bool bannerNeeded = false; // attended on a `will` record: show the banner before the call
};

struct PairActions {
    Action upgrade; Action schedule; Action clearSchedule;
    QString modeDefault;
};

constexpr qint64 ScheduleMinLeadSeconds = 120; // panel.rs SCHEDULE_MIN_LEAD

DriveActions deriveActions(const DriveView &d, const PairView &p, const GuiFacts &g);
PairActions derivePairActions(const RecoveryDocument &doc, const GuiFacts &g);

// Words for the cards.
QString ageWords(std::optional<qint64> epoch, qint64 nowEpoch); // "3 days ago" | "unknown"
QString verdictWords(const QString &verdict);                   // will → "will run btrbk at boot", may → "may run btrbk at boot", no → "does not run btrbk at boot", "" → "unknown"
QString sessionWords(const SessionView &s, qint64 nowEpoch);    // "running as helper job job-7 (VM running)" …
QString scheduleWords(const ScheduleView &s);                   // "<state>: <detail>"
```

- [ ] **Step 1: Write the failing tests** (same block as Task 3; a helper builds the fixture's drive A as the enabling baseline and mutates it per rule):

```cpp
    struct Scenario {
        RecoveryDocument doc;
        GuiFacts facts;
        DriveView &a() { return doc.drives[0]; }
        DriveView &b() { return doc.drives[1]; }
    };

    // Drive A of the fixture, with its running job and pending schedule
    // removed: a drive on which everything is allowed. nowEpoch is the
    // fixture's clock.
    static Scenario idleA()
    {
        QString err;
        Scenario s{*RecoveryDocument::parse(fixture(), &err), {}};
        s.a().session.reset();
        s.a().schedule.reset();
        s.facts.nowEpoch = 1791500000;
        s.facts.chosenNow = true;
        s.facts.viewerInstalled = true;
        return s;
    }

    void upgradeAttendedNeedsARecordAndNoSession()
    {
        Scenario s = idleA();
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY2(a.upgrade.enabled, qPrintable(a.upgrade.why));
        QVERIFY(!a.bannerNeeded);
        QVERIFY(a.upgrade.why.contains(QStringLiteral("attended")));

        // No record: refused with the record's error
        Scenario r = idleA();
        r.a().record.reset();
        r.a().recordError = QStringLiteral("the record of system-recovery-A-2tb holds no reading");
        a = deriveActions(r.a(), r.doc.pair, r.facts);
        QVERIFY(!a.upgrade.enabled);
        QCOMPARE(a.upgrade.why, *r.a().recordError);

        // A session on this drive
        Scenario h = idleA();
        h.a().session = SessionView{QStringLiteral("other:recovery-os VM session system-recovery-A-2tb pid 1"), 1791400000, QStringLiteral("running"), std::nullopt};
        a = deriveActions(h.a(), h.doc.pair, h.facts);
        QVERIFY(!a.upgrade.enabled);
        QVERIFY(a.upgrade.why.contains(QStringLiteral("a session holds")));

        // A pair session
        Scenario pr = idleA();
        pr.doc.pair.session = SessionView{QStringLiteral("unit:das-recovery-os-update-both.service"), 1791400000, QString(), false};
        a = deriveActions(pr.a(), pr.doc.pair, pr.facts);
        QVERIFY(!a.upgrade.enabled);
        QVERIFY(a.upgrade.why.contains(QStringLiteral("both drives")));

        // This window's own job, on the other drive
        Scenario j = idleA();
        j.facts.ownJobId = QStringLiteral("job-9");
        a = deriveActions(j.a(), j.doc.pair, j.facts);
        QVERIFY(!a.upgrade.enabled);
        QVERIFY(a.upgrade.why.contains(QStringLiteral("this window")));
    }

    void upgradeUnattendedFollowsTheDocumentsPossible()
    {
        Scenario s = idleA();
        s.facts.unattended = true;
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY2(a.upgrade.enabled, qPrintable(a.upgrade.why));
        QVERIFY(a.upgrade.why.contains(QStringLiteral("unattended")));

        s.a().unattendedPossible = false;
        s.a().unattendedWhy = QStringLiteral("the boot record says btrbk will run");
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.upgrade.enabled);
        QCOMPARE(a.upgrade.why, s.a().unattendedWhy);
    }

    void theBannerIsNeededForAttendedOnAWillRecordOnly()
    {
        Scenario s = idleA();
        s.a().verdict = QStringLiteral("will");
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(a.upgrade.enabled);
        QVERIFY(a.bannerNeeded);
        s.facts.unattended = true;
        s.a().unattendedPossible = false;
        s.a().unattendedWhy = QStringLiteral("the boot record says btrbk will run");
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.upgrade.enabled);
        QVERIFY(!a.bannerNeeded);
        s.facts.unattended = false;
        s.a().verdict = QStringLiteral("may");
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).bannerNeeded);
    }

    void scheduleNeedsATimeTwoMinutesAhead()
    {
        Scenario s = idleA();
        s.facts.unattended = true;
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.schedule.enabled); // Now is ticked
        QVERIFY(a.schedule.why.contains(QStringLiteral("untick Now")));

        s.facts.chosenNow = false;
        s.facts.chosenEpoch = s.facts.nowEpoch + ScheduleMinLeadSeconds - 1;
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.schedule.enabled);
        QVERIFY(a.schedule.why.contains(QStringLiteral("2 minutes")));

        s.facts.chosenEpoch = s.facts.nowEpoch - 3600;
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).schedule.enabled);

        s.facts.chosenEpoch = s.facts.nowEpoch + ScheduleMinLeadSeconds;
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY2(a.schedule.enabled, qPrintable(a.schedule.why));

        // A running session does not block scheduling; the attended radio does not either
        s.a().session = SessionView{QStringLiteral("other:x"), std::nullopt, QString(), std::nullopt};
        s.facts.unattended = false;
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).schedule.enabled);

        // unattended impossible blocks it
        s.a().unattendedPossible = false;
        s.a().unattendedWhy = QStringLiteral("no guest agent");
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.schedule.enabled);
        QCOMPARE(a.schedule.why, s.a().unattendedWhy);
    }

    void clearScheduleNeedsAPendingOrMissedOne()
    {
        Scenario s = idleA();
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).clearSchedule.enabled);
        s.a().schedule = ScheduleView{QStringLiteral("u.timer"), 1791600000, QString(), QStringLiteral("pending"), QString()};
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).clearSchedule.enabled);
        s.a().schedule->state = QStringLiteral("missed");
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).clearSchedule.enabled);
        s.a().schedule->state = QStringLiteral("fired");
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).clearSchedule.enabled);
        s.a().schedule->state = QStringLiteral("running");
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).clearSchedule.enabled);
    }

    void consoleNeedsAnAttendedSessionThisWindowKnows()
    {
        Scenario s = idleA();
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).console.enabled);

        // The document says attended and the VM runs
        s.a().session = SessionView{QStringLiteral("other:x"), std::nullopt, QStringLiteral("running"), true};
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).console.enabled);
        s.a().session->domainState = QStringLiteral("shut off");
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).console.enabled);

        // A job session: attended is null; only this window's attended job qualifies
        s.a().session = SessionView{QStringLiteral("job:job-7"), std::nullopt, QStringLiteral("running"), std::nullopt};
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.console.enabled);
        QVERIFY(a.console.why.contains(QStringLiteral("cannot tell")));
        s.facts.ownJobId = QStringLiteral("job-7");
        s.facts.ownJobAttended = false;
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).console.enabled);
        s.facts.ownJobAttended = true;
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).console.enabled);

        // The viewer missing keeps the button enabled (the click shows the path)
        s.facts.viewerInstalled = false;
        a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(a.console.enabled);
        QVERIFY(a.console.why.contains(QStringLiteral("virt-viewer")));
    }

    void endSessionIsForAHolderThatIsNeitherAJobNorAUnit()
    {
        Scenario s = idleA();
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).endSession.enabled);
        s.a().session = SessionView{QStringLiteral("other:recovery-os VM session system-recovery-A-2tb pid 1"), std::nullopt, QString(), std::nullopt};
        QVERIFY(deriveActions(s.a(), s.doc.pair, s.facts).endSession.enabled);
        s.a().session->by = QStringLiteral("job:job-7");
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.endSession.enabled);
        QVERIFY(a.endSession.why.contains(QStringLiteral("Cancel")));
        s.a().session->by = QStringLiteral("unit:das-recovery-os-update-system-recovery-A-2tb.service");
        QVERIFY(!deriveActions(s.a(), s.doc.pair, s.facts).endSession.enabled);
    }

    void anErrorInAPartDisablesWhatDependsOnIt()
    {
        Scenario s = idleA();
        s.a().sessionError = QStringLiteral("das-recovery-os-update-both.service cannot be read: boom");
        s.a().session = SessionView{QStringLiteral("other:x"), std::nullopt, QStringLiteral("running"), true};
        DriveActions a = deriveActions(s.a(), s.doc.pair, s.facts);
        QVERIFY(!a.upgrade.enabled);
        QCOMPARE(a.upgrade.why, *s.a().sessionError);
        QVERIFY(!a.console.enabled);
        QCOMPARE(a.console.why, *s.a().sessionError);
        QVERIFY(!a.endSession.enabled);
        QCOMPARE(a.endSession.why, *s.a().sessionError);

        Scenario t = idleA();
        t.a().scheduleError = QStringLiteral("x.timer cannot be read: boom");
        t.a().schedule = ScheduleView{QStringLiteral("x.timer"), std::nullopt, QString(), QStringLiteral("pending"), QString()};
        t.facts.unattended = true; t.facts.chosenNow = false; t.facts.chosenEpoch = t.facts.nowEpoch + 600;
        a = deriveActions(t.a(), t.doc.pair, t.facts);
        QVERIFY(!a.schedule.enabled);
        QCOMPARE(a.schedule.why, *t.a().scheduleError);
        QVERIFY(!a.clearSchedule.enabled);

        // The pair's session error blocks the drive's upgrade too
        Scenario u = idleA();
        u.doc.pair.sessionError = QStringLiteral("pair boom");
        QVERIFY(!deriveActions(u.a(), u.doc.pair, u.facts).upgrade.enabled);
    }

    void pairActionsNeedBothDrivesAndNoPairError()
    {
        Scenario s = idleA();
        s.facts.unattended = true;
        // Drive B of the fixture is all errors: the pair is refused with B's reason
        PairActions p = derivePairActions(s.doc, s.facts);
        QVERIFY(!p.upgrade.enabled);
        QVERIFY(p.upgrade.why.contains(QStringLiteral("system-recovery-B-2tb")));
        QCOMPARE(p.modeDefault, QStringLiteral("sequential"));

        // Make B a copy of A: allowed
        s.b() = s.a();
        s.b().label = QStringLiteral("system-recovery-B-2tb");
        p = derivePairActions(s.doc, s.facts);
        QVERIFY2(p.upgrade.enabled, qPrintable(p.upgrade.why));
        s.facts.chosenNow = false; s.facts.chosenEpoch = s.facts.nowEpoch + 600;
        QVERIFY(derivePairActions(s.doc, s.facts).schedule.enabled);

        s.doc.pair.scheduleError = QStringLiteral("both boom");
        p = derivePairActions(s.doc, s.facts);
        QVERIFY(!p.schedule.enabled);
        QCOMPARE(p.schedule.why, *s.doc.pair.scheduleError);

        // One drive only: no pair
        s.doc.drives.removeLast();
        QVERIFY(!derivePairActions(s.doc, s.facts).upgrade.enabled);
    }

    void wordsForTheCards()
    {
        QCOMPARE(ageWords(std::nullopt, 100), QStringLiteral("unknown"));
        QCOMPARE(ageWords(1791500000 - 3 * 86400, 1791500000), QStringLiteral("3 days ago"));
        QCOMPARE(ageWords(1791500000 - 3600, 1791500000), QStringLiteral("today"));
        QCOMPARE(ageWords(1791500000 - 86400, 1791500000), QStringLiteral("1 day ago"));
        QCOMPARE(verdictWords(QStringLiteral("will")), QStringLiteral("will run btrbk at boot"));
        QCOMPARE(verdictWords(QString()), QStringLiteral("unknown"));
        QCOMPARE(sessionWords(SessionView{QStringLiteral("job:job-7"), std::nullopt, QStringLiteral("running"), std::nullopt}, 0),
                 QStringLiteral("running as helper job job-7 (VM running)"));
        QCOMPARE(sessionWords(SessionView{QStringLiteral("unit:x.service"), 1791500000 - 600, QString(), false}, 1791500000),
                 QStringLiteral("running as scheduled unit x.service since 10 minutes ago, unattended (VM state unknown)"));
        QCOMPARE(scheduleWords(ScheduleView{QStringLiteral("u"), std::nullopt, QString(), QStringLiteral("missed"), QStringLiteral("The time passed.")}),
                 QStringLiteral("missed: The time passed."));
    }
```

- [ ] **Step 2: Build to see them fail**

Run: `cmake --build /tmp/das-8249-s3-build --target gui-smoketest 2>&1 | grep -m3 error`
Expected: `'deriveActions' was not declared`.

- [ ] **Step 3: Implement** (append to `recoverystatus.cpp`):

```cpp
namespace {

// The first failing condition's words, or empty when all pass. Order is the
// spec's §4 table: the cautious side wins, the most specific reason first.
QString sessionBlocker(const DriveView &d, const PairView &p, const GuiFacts &g)
{
    if (d.sessionError)
        return *d.sessionError;
    if (p.sessionError)
        return *p.sessionError;
    if (d.session)
        return QStringLiteral("a session holds %1 (%2)").arg(d.displayName, sessionWords(*d.session, g.nowEpoch));
    if (p.session)
        return QStringLiteral("a session holds both drives (%1)").arg(sessionWords(*p.session, g.nowEpoch));
    if (!g.ownJobId.isEmpty())
        return QStringLiteral("this window's session %1 is running").arg(g.ownJobId);
    return {};
}

QString unattendedBlocker(const DriveView &d)
{
    if (!d.unattendedPossible)
        return d.unattendedWhy.isEmpty() ? QStringLiteral("the record does not admit an unattended session") : d.unattendedWhy;
    return {};
}

QString timeBlocker(const GuiFacts &g)
{
    if (g.chosenNow)
        return QStringLiteral("untick Now to schedule a time");
    if (g.chosenEpoch < g.nowEpoch + ScheduleMinLeadSeconds)
        return QStringLiteral("pick a time at least 2 minutes ahead");
    return {};
}

Action allowed(const QString &does) { return {true, does}; }
Action refused(const QString &why) { return {false, why}; }

} // namespace

DriveActions deriveActions(const DriveView &d, const PairView &p, const GuiFacts &g)
{
    DriveActions a;

    // Upgrade
    QString why = d.recordError ? *d.recordError : sessionBlocker(d, p, g);
    if (why.isEmpty() && !d.record)
        why = QStringLiteral("no record of this drive yet");
    if (why.isEmpty() && g.unattended)
        why = unattendedBlocker(d);
    if (!why.isEmpty()) {
        a.upgrade = refused(why);
    } else if (g.unattended) {
        a.upgrade = allowed(QStringLiteral("Update %1 now, unattended: the recovery OS boots in its VM and upgrades itself through its guest agent, then powers off").arg(d.displayName));
    } else {
        a.upgrade = allowed(QStringLiteral("Update %1 now, attended: the recovery OS boots in its VM and the console opens for you to log in and run the checklist").arg(d.displayName));
        a.bannerNeeded = d.verdict == QLatin1String("will");
    }

    // Schedule (a running session does not block it; the radio does not either)
    why = d.scheduleError ? *d.scheduleError : (d.recordError ? *d.recordError : unattendedBlocker(d));
    if (why.isEmpty())
        why = timeBlocker(g);
    a.schedule = why.isEmpty()
        ? allowed(QStringLiteral("Schedule an unattended update of %1 at the chosen time").arg(d.displayName))
        : refused(why);

    // Clear schedule
    if (d.scheduleError)
        a.clearSchedule = refused(*d.scheduleError);
    else if (d.schedule && (d.schedule->state == QLatin1String("pending") || d.schedule->state == QLatin1String("missed")))
        a.clearSchedule = allowed(QStringLiteral("Remove the schedule %1").arg(d.schedule->unit));
    else
        a.clearSchedule = refused(QStringLiteral("no schedule to clear"));

    // Console
    if (d.sessionError) {
        a.console = refused(*d.sessionError);
    } else if (!d.session) {
        a.console = refused(QStringLiteral("no session is running"));
    } else if (d.session->domainState != QLatin1String("running")) {
        a.console = refused(QStringLiteral("the VM is not running (%1)").arg(d.session->domainState.isEmpty() ? QStringLiteral("state unknown") : d.session->domainState));
    } else {
        const bool ownJob = d.session->by == QStringLiteral("job:") + g.ownJobId && !g.ownJobId.isEmpty();
        const bool attended = d.session->attended.value_or(false) || (ownJob && g.ownJobAttended);
        if (!attended && !d.session->attended.has_value() && !ownJob)
            a.console = refused(QStringLiteral("cannot tell whether this session is attended (it was not started by this window)"));
        else if (!attended)
            a.console = refused(QStringLiteral("an unattended session has no console to attend"));
        else if (g.viewerInstalled)
            a.console = allowed(QStringLiteral("Open the recovery OS's console in remote-viewer"));
        else
            a.console = allowed(QStringLiteral("virt-viewer is not installed: shows the console socket's path to connect a VNC viewer by hand"));
    }

    // End session
    if (d.sessionError)
        a.endSession = refused(*d.sessionError);
    else if (!d.session)
        a.endSession = refused(QStringLiteral("no session to end"));
    else if (d.session->by.startsWith(QLatin1String("job:")))
        a.endSession = refused(QStringLiteral("the session is a running job — use Cancel in the progress panel"));
    else if (d.session->by.startsWith(QLatin1String("unit:")))
        a.endSession = refused(QStringLiteral("a scheduled session is running; it gives the drive back itself"));
    else
        a.endSession = allowed(QStringLiteral("Give %1 back: power off its VM if it still runs, remove the guard and the console bridge, release the lock").arg(d.displayName));

    return a;
}

PairActions derivePairActions(const RecoveryDocument &doc, const GuiFacts &g)
{
    PairActions p;
    p.modeDefault = doc.pair.modeDefault;
    if (doc.drives.size() != 2) {
        const Action no = refused(QStringLiteral("both drives: the configuration has %1 recovery drive(s), not 2").arg(doc.drives.size()));
        p.upgrade = p.schedule = p.clearSchedule = no;
        return p;
    }
    for (const DriveView &d : doc.drives) {
        const DriveActions a = deriveActions(d, doc.pair, g);
        if (!a.upgrade.enabled && !p.upgrade.enabled && p.upgrade.why.isEmpty())
            p.upgrade = refused(QStringLiteral("%1: %2").arg(d.label, a.upgrade.why));
        if (!a.schedule.enabled && !p.schedule.enabled && p.schedule.why.isEmpty())
            p.schedule = refused(QStringLiteral("%1: %2").arg(d.label, a.schedule.why));
    }
    if (doc.pair.sessionError)
        p.upgrade = refused(*doc.pair.sessionError);
    else if (p.upgrade.why.isEmpty())
        p.upgrade = allowed(QStringLiteral("Update both drives now, %1").arg(g.unattended ? QStringLiteral("unattended") : QStringLiteral("attended")));
    if (doc.pair.scheduleError)
        p.schedule = refused(*doc.pair.scheduleError);
    else if (p.schedule.why.isEmpty())
        p.schedule = allowed(QStringLiteral("Schedule an unattended update of both drives at the chosen time"));
    if (doc.pair.scheduleError)
        p.clearSchedule = refused(*doc.pair.scheduleError);
    else if (doc.pair.schedule && (doc.pair.schedule->state == QLatin1String("pending") || doc.pair.schedule->state == QLatin1String("missed")))
        p.clearSchedule = allowed(QStringLiteral("Remove the schedule %1").arg(doc.pair.schedule->unit));
    else
        p.clearSchedule = refused(QStringLiteral("no schedule to clear"));
    return p;
}

QString ageWords(std::optional<qint64> epoch, qint64 nowEpoch)
{
    if (!epoch)
        return QStringLiteral("unknown");
    const qint64 days = (nowEpoch - *epoch) / 86400;
    if (days <= 0)
        return QStringLiteral("today");
    return days == 1 ? QStringLiteral("1 day ago") : QStringLiteral("%1 days ago").arg(days);
}

QString verdictWords(const QString &verdict)
{
    if (verdict == QLatin1String("will")) return QStringLiteral("will run btrbk at boot");
    if (verdict == QLatin1String("may")) return QStringLiteral("may run btrbk at boot");
    if (verdict == QLatin1String("no")) return QStringLiteral("does not run btrbk at boot");
    return QStringLiteral("unknown");
}

QString sessionWords(const SessionView &s, qint64 nowEpoch)
{
    QString who;
    if (s.by.startsWith(QLatin1String("job:")))
        who = QStringLiteral("running as helper job %1").arg(s.by.mid(4));
    else if (s.by.startsWith(QLatin1String("unit:")))
        who = QStringLiteral("running as scheduled unit %1").arg(s.by.mid(5));
    else
        who = QStringLiteral("held by %1").arg(s.by.startsWith(QLatin1String("other:")) ? s.by.mid(6) : s.by);
    if (s.sinceEpoch) {
        const qint64 minutes = (nowEpoch - *s.sinceEpoch) / 60;
        who += QStringLiteral(" since %1 minutes ago").arg(minutes);
    }
    if (s.attended.has_value())
        who += *s.attended ? QStringLiteral(", attended") : QStringLiteral(", unattended");
    who += s.domainState.isEmpty() ? QStringLiteral(" (VM state unknown)") : QStringLiteral(" (VM %1)").arg(s.domainState);
    return who;
}

QString scheduleWords(const ScheduleView &s)
{
    return QStringLiteral("%1: %2").arg(s.state, s.detail);
}
```

- [ ] **Step 4: Run the GUI suite**

Run: `cmake --build /tmp/das-8249-s3-build && ctest --test-dir /tmp/das-8249-s3-build -R gui-smoketest --output-on-failure 2>&1 | grep -E 'PASS|FAIL|Totals'`
Expected: every new case PASS; `Totals: … 0 failed`. A failing `wordsForTheCards` on exact strings means the implementation and the test disagree on wording — the test's words are the spec, change the implementation.

- [ ] **Step 5: Counter-test** — in `deriveActions`, make the console branch `const bool attended = true;`: `consoleNeedsAnAttendedSessionThisWindowKnows` must FAIL. Delete the `if (d.sessionError)` line from the Upgrade block: `anErrorInAPartDisablesWhatDependsOnIt` must FAIL. Restore both.

- [ ] **Step 6: Commit**

```bash
git add gui/src/recoverystatus.h gui/src/recoverystatus.cpp gui/tests/smoketest.cpp
git commit -S -m "$(cat <<'EOF'
gui: the recovery panel's rules, pure and pinned both ways

deriveActions and derivePairActions turn the document plus what the
window knows into {enabled, why} per button, the cautious side first.
Every row of the spec's table has an enabling and a refusing case.
EOF
)"
```

---

### Task 5: `DBusClient` — five wrappers, the session-end timeout, the console command

**Files:**
- Modify: `gui/src/dbusclient.h`, `gui/src/dbusclient.cpp`
- Modify: `gui/tests/smoketest.cpp`

**Interfaces:**
- Produces (`dbusclient.h`, in the public section and the signals):

```cpp
    // --- Recovery drives (bd DAS-Backup-Manager-8249 stage 3) ---
    void recoveryOsStatusAsync();                                   // → recoveryOsStatusResult(json); empty on error/unavailable
    void recoveryOsSession(const QStringList &labels, bool unattended,
                           const QString &mode);                    // a job: jobStarted(id, "Recovery OS session"); never passes accept_boot_record_risk
    void recoveryOsSessionEnd(const QString &label);                // → recoveryOsSessionEndResult(label, ok, lines); 10-minute timeout
    void recoveryOsScheduleSet(const QStringList &labels, qint64 atEpoch,
                               const QString &mode);                // → recoveryOsScheduleResult(unit); atEpoch 0 clears
    void recoveryOsConsole(const QString &label);                   // → recoveryOsConsoleResult(label, path)
    // The viewer to launch for a console socket: program and arguments. Pure,
    // so the test can pin it without a socket.
    [[nodiscard]] static QPair<QString, QStringList> consoleCommand(const QString &socketPath);
    static constexpr int SessionEndTimeoutMs = 600000;
    [[nodiscard]] int sessionEndTimeoutMs() const; // what the session-end interface is really set to

Q_SIGNALS:
    void recoveryOsStatusResult(const QString &json);
    void recoveryOsSessionEndResult(const QString &label, bool ok, const QString &lines);
    void recoveryOsScheduleResult(const QString &unit);
    void recoveryOsConsoleResult(const QString &label, const QString &path);
```

and a second `QDBusInterface *m_slowInterface` (same service, path and name) whose timeout is set to `SessionEndTimeoutMs` in the constructor — `QDBusAbstractInterface::setTimeout` is per interface object, and the other calls keep the default.

- [ ] **Step 1: Write the failing tests**

```cpp
    void consoleCommandIsRemoteViewerOnTheUnixSocket()
    {
        const auto [program, args] = DBusClient::consoleCommand(QStringLiteral("/run/das-recovery-os-vm/1000/system-recovery-A-2tb.vnc"));
        QCOMPARE(program, QStringLiteral("remote-viewer"));
        QCOMPARE(args, QStringList{QStringLiteral("vnc+unix:///run/das-recovery-os-vm/1000/system-recovery-A-2tb.vnc")});
    }

    void sessionEndUsesATenMinuteTimeout()
    {
        DBusClient client;
        QCOMPARE(client.sessionEndTimeoutMs(), 600000);
    }

    void recoveryCallsOnAnUnavailableHelperAnswerEmptyNotSilent()
    {
        DBusClient client;
        if (client.isAvailable())
            QSKIP("a helper is reachable; this pins the unavailable path");
        QSignalSpy status(&client, &DBusClient::recoveryOsStatusResult);
        client.recoveryOsStatusAsync();
        QCOMPARE(status.count(), 1);
        QVERIFY(status.at(0).at(0).toString().isEmpty());
        QSignalSpy end(&client, &DBusClient::recoveryOsSessionEndResult);
        client.recoveryOsSessionEnd(QStringLiteral("system-recovery-A-2tb"));
        QCOMPARE(end.count(), 1);
        QVERIFY(!end.at(0).at(1).toBool());
    }
```

- [ ] **Step 2: Build to see them fail**

Run: `cmake --build /tmp/das-8249-s3-build --target gui-smoketest 2>&1 | grep -m2 error`
Expected: `'consoleCommand' is not a member of 'DBusClient'`.

- [ ] **Step 3: Implement** (`dbusclient.cpp`; the constructor creates `m_slowInterface` beside `m_interface` and calls `m_slowInterface->setTimeout(SessionEndTimeoutMs)`; the header declares `QDBusInterface *m_slowInterface = nullptr;`):

```cpp
// --- Recovery drives ---

void DBusClient::recoveryOsStatusAsync()
{
    if (!m_available) {
        Q_EMIT recoveryOsStatusResult({});
        return;
    }
    QDBusPendingCall pending = m_interface->asyncCall(QStringLiteral("RecoveryOsStatus"));
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w) {
        QDBusPendingReply<QString> reply = *w;
        if (reply.isError()) {
            Q_EMIT errorOccurred(QStringLiteral("RecoveryOsStatus"),
                                 mapDBusError(reply.error().name(), reply.error().message()));
            Q_EMIT recoveryOsStatusResult({});
        } else {
            Q_EMIT recoveryOsStatusResult(reply.value());
        }
        w->deleteLater();
    });
}

void DBusClient::recoveryOsSession(const QStringList &labels, bool unattended, const QString &mode)
{
    // accept_boot_record_risk is always false from the GUI (spec §3.3).
    callAsync(QStringLiteral("RecoveryOsSession"),
              {QVariant::fromValue(labels), unattended, mode, false},
              QStringLiteral("Recovery OS session"));
}

void DBusClient::recoveryOsSessionEnd(const QString &label)
{
    if (!m_available) {
        Q_EMIT recoveryOsSessionEndResult(label, false, unavailableReason());
        return;
    }
    QDBusPendingCall pending = m_slowInterface->asyncCall(QStringLiteral("RecoveryOsSessionEnd"), label);
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, label](QDBusPendingCallWatcher *w) {
        QDBusPendingReply<bool, QString> reply = *w;
        if (reply.isError()) {
            const QString text = mapDBusError(reply.error().name(), reply.error().message());
            Q_EMIT errorOccurred(QStringLiteral("RecoveryOsSessionEnd"), text);
            Q_EMIT recoveryOsSessionEndResult(label, false, text);
        } else {
            Q_EMIT recoveryOsSessionEndResult(label, reply.argumentAt<0>(), reply.argumentAt<1>());
        }
        w->deleteLater();
    });
}

void DBusClient::recoveryOsScheduleSet(const QStringList &labels, qint64 atEpoch, const QString &mode)
{
    if (!m_available) {
        Q_EMIT errorOccurred(QStringLiteral("RecoveryOsScheduleSet"), unavailableReason());
        return;
    }
    QDBusPendingCall pending = m_interface->asyncCallWithArgumentList(
        QStringLiteral("RecoveryOsScheduleSet"), {QVariant::fromValue(labels), atEpoch, mode});
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this](QDBusPendingCallWatcher *w) {
        QDBusPendingReply<QString> reply = *w;
        if (reply.isError())
            Q_EMIT errorOccurred(QStringLiteral("RecoveryOsScheduleSet"),
                                 mapDBusError(reply.error().name(), reply.error().message()));
        else
            Q_EMIT recoveryOsScheduleResult(reply.value());
        w->deleteLater();
    });
}

void DBusClient::recoveryOsConsole(const QString &label)
{
    if (!m_available) {
        Q_EMIT errorOccurred(QStringLiteral("RecoveryOsConsole"), unavailableReason());
        return;
    }
    QDBusPendingCall pending = m_interface->asyncCall(QStringLiteral("RecoveryOsConsole"), label);
    auto *watcher = new QDBusPendingCallWatcher(pending, this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, label](QDBusPendingCallWatcher *w) {
        QDBusPendingReply<QString> reply = *w;
        if (reply.isError())
            Q_EMIT errorOccurred(QStringLiteral("RecoveryOsConsole"),
                                 mapDBusError(reply.error().name(), reply.error().message()));
        else
            Q_EMIT recoveryOsConsoleResult(label, reply.value());
        w->deleteLater();
    });
}

QPair<QString, QStringList> DBusClient::consoleCommand(const QString &socketPath)
{
    return {QStringLiteral("remote-viewer"), {QStringLiteral("vnc+unix://") + socketPath}};
}

int DBusClient::sessionEndTimeoutMs() const
{
    return m_slowInterface ? m_slowInterface->timeout() : -1;
}
```

`atEpoch` is `qint64`; the helper declares `at_epoch: i64`, so pass it as `QVariant(qlonglong(atEpoch))` — a bare `qint64` in a `QList<QVariant>` initialiser is fine on Qt 6, but check the signature the bus sees with `busctl introspect` in Task 8 (`x`).

- [ ] **Step 4: Run the suite**

Run: `cmake --build /tmp/das-8249-s3-build && ctest --test-dir /tmp/das-8249-s3-build -R gui-smoketest --output-on-failure 2>&1 | grep -E 'consoleCommand|sessionEnd|recoveryCalls|Totals'`
Expected: three PASS, 0 failed.

- [ ] **Step 5: Counter-test** — set the constructor's timeout to `25000`: `sessionEndUsesATenMinuteTimeout` FAILs; restore.

- [ ] **Step 6: Commit**

```bash
git add gui/src/dbusclient.h gui/src/dbusclient.cpp gui/tests/smoketest.cpp
git commit -S -m "$(cat <<'EOF'
gui: DBusClient wraps the five RecoveryOs* methods

RecoveryOsSessionEnd goes through a second interface object with a
10-minute timeout (bd k84b: a give-back can outlast Qt's 25 s);
RecoveryOsSession is a job and never passes accept_boot_record_risk;
consoleCommand is remote-viewer on vnc+unix://.
EOF
)"
```

---

### Task 6: the panel, the sidebar section, page 5

**Files:**
- Create: `gui/src/recoverypanel.h`, `gui/src/recoverypanel.cpp`
- Modify: `gui/src/sidebar.h` (enum), `gui/src/sidebar.cpp` (`buildTree`), `gui/src/mainwindow.h`, `gui/src/mainwindow.cpp` (page 5, `onSectionChanged`), `gui/CMakeLists.txt` (both source lists)
- Modify: `gui/tests/smoketest.cpp`

**Interfaces:**
- Consumes: Task 3's `RecoveryDocument`, Task 4's `deriveActions`/`derivePairActions`/words, Task 5's wrappers and signals.
- Produces (`recoverypanel.h`):

```cpp
#pragma once
#include <QWidget>
#include "recoverystatus.h"

class QCheckBox; class QComboBox; class QDateTimeEdit; class QGroupBox; class QLabel;
class QPushButton; class QRadioButton; class QTimer; class QVBoxLayout;
class DBusClient;

class RecoveryPanel : public QWidget
{
    Q_OBJECT
public:
    explicit RecoveryPanel(DBusClient *client, QWidget *parent = nullptr);

    // Test seams: the document as the helper would send it, and the facts.
    void applyDocument(const QByteArray &json);
    [[nodiscard]] QString statusLine() const;     // the line under the cards (parse errors, job outcomes)
    [[nodiscard]] GuiFacts facts() const;

public Q_SLOTS:
    void refresh();            // one RecoveryOsStatus in flight at a time
    void setShown(bool shown); // starts/stops the 30 s timer

private Q_SLOTS:
    void onStatusResult(const QString &json);
    void onJobStarted(const QString &jobId, const QString &operation);
    void onJobFinished(const QString &jobId, bool success, const QString &summary);
    void onSessionEndResult(const QString &label, bool ok, const QString &lines);
    void onScheduleResult(const QString &unit);
    void onConsoleResult(const QString &label, const QString &path);
    void onUpgrade();
    void onSchedule();
    void onClearSchedule();
    void onConsole();
    void onEndSession();
    void rederive();

private:
    struct Card { QGroupBox *box; QLabel *lines; QLabel *errors; };
    void buildControls();
    void rebuildCards();
    QStringList selectedLabels() const; // one label, or both in document order
    bool selectionIsBoth() const;
    DriveView *selectedDrive();         // nullptr for "both"

    DBusClient *m_client;
    std::optional<RecoveryDocument> m_doc;
    QString m_parseError;
    QVBoxLayout *m_cards = nullptr;
    QList<Card> m_cardWidgets;
    QComboBox *m_selector = nullptr;
    QRadioButton *m_attended = nullptr; QRadioButton *m_unattended = nullptr;
    QRadioButton *m_sequential = nullptr; QRadioButton *m_parallel = nullptr;
    QCheckBox *m_now = nullptr; QDateTimeEdit *m_when = nullptr;
    QPushButton *m_upgrade = nullptr; QPushButton *m_schedule = nullptr; QPushButton *m_clear = nullptr;
    QPushButton *m_console = nullptr; QPushButton *m_end = nullptr;
    QLabel *m_status = nullptr;
    QTimer *m_timer = nullptr;
    bool m_refreshInFlight = false;
    QString m_ownJobId; bool m_ownJobAttended = false;
    QSet<QString> m_earlyFinished; // JobFinished ids seen before jobStarted named the job
    bool m_jobRequested = false;
    bool m_endInFlight = false;
};
```

- [ ] **Step 1: Write the failing tests** (`#include "../src/recoverypanel.h"`; the `partsOf`-style finder uses `objectName`s the panel sets: `upgrade`, `schedule`, `clearSchedule`, `console`, `endSession`, `selector`, `attended`, `unattended`, `now`, `when`):

```cpp
    struct RecoveryParts {
        QPushButton *upgrade, *schedule, *clear, *console, *end;
        QComboBox *selector; QRadioButton *attended, *unattended; QCheckBox *now; QDateTimeEdit *when;
        bool ok() const { return upgrade && schedule && clear && console && end && selector && attended && unattended && now && when; }
    };
    static RecoveryParts recoveryPartsOf(QWidget &w)
    {
        return {w.findChild<QPushButton *>(QStringLiteral("upgrade")), w.findChild<QPushButton *>(QStringLiteral("schedule")),
                w.findChild<QPushButton *>(QStringLiteral("clearSchedule")), w.findChild<QPushButton *>(QStringLiteral("console")),
                w.findChild<QPushButton *>(QStringLiteral("endSession")), w.findChild<QComboBox *>(QStringLiteral("selector")),
                w.findChild<QRadioButton *>(QStringLiteral("attended")), w.findChild<QRadioButton *>(QStringLiteral("unattended")),
                w.findChild<QCheckBox *>(QStringLiteral("now")), w.findChild<QDateTimeEdit *>(QStringLiteral("when"))};
    }

    void recoveryPanelConstructsWithoutAHelperAndDisablesEverything()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());
        for (QPushButton *b : {p.upgrade, p.schedule, p.clear, p.console, p.end}) {
            QVERIFY(!b->isEnabled());
            QVERIFY(!b->toolTip().isEmpty());
        }
    }

    void recoveryPanelBindsTheDocument()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        panel.applyDocument(fixture());
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());
        QCOMPARE(p.selector->count(), 3); // A, B, both
        QCOMPARE(p.selector->itemText(2), QStringLiteral("Both drives"));
        QVERIFY(p.attended->isChecked());
        QVERIFY(p.now->isChecked());
        QVERIFY(!p.when->isEnabled());

        // Drive A has a running job and a pending schedule in the fixture
        p.selector->setCurrentIndex(0);
        QVERIFY(!p.upgrade->isEnabled());
        QVERIFY(p.upgrade->toolTip().contains(QStringLiteral("a session holds")));
        QVERIFY(p.clear->isEnabled());
        QVERIFY(!p.end->isEnabled());
        QVERIFY(p.end->toolTip().contains(QStringLiteral("Cancel")));

        // Drive B is all errors
        p.selector->setCurrentIndex(1);
        QVERIFY(!p.upgrade->isEnabled());
        QVERIFY(p.upgrade->toolTip().contains(QStringLiteral("no record")));
        QVERIFY(!p.clear->isEnabled());

        // The cards show every error in its place
        const auto labels = panel.findChildren<QLabel *>();
        bool sawSessionError = false;
        for (QLabel *l : labels)
            if (l->text().contains(QStringLiteral("cannot be read")))
                sawSessionError = true;
        QVERIFY(sawSessionError);
    }

    void panelShowsTheParseErrorAndDisablesEverything()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        panel.applyDocument(fixture());
        panel.applyDocument("{\"schema\": 2}");
        QVERIFY(panel.statusLine().contains(QStringLiteral("schema 2")));
        const RecoveryParts p = recoveryPartsOf(panel);
        for (QPushButton *b : {p.upgrade, p.schedule, p.clear, p.console, p.end})
            QVERIFY(!b->isEnabled());
        // A good document afterwards recovers it
        panel.applyDocument(fixture());
        QVERIFY(!panel.statusLine().contains(QStringLiteral("schema 2")));
        p.selector->setCurrentIndex(0);
        QVERIFY(p.clear->isEnabled());
    }

    void aWarnedSessionNeedsALookAFailedOneFailed()
    {
        DBusClient client; // unavailable: a click reaches no helper, but the request is remembered
        RecoveryPanel panel(&client);
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());

        requestSession(panel, p);
        QVERIFY(!p.upgrade->isEnabled());
        panel.onJobStarted(QStringLiteral("job-1"), QStringLiteral("Recovery OS session"));
        panel.onJobFinished(QStringLiteral("job-1"), false, QStringLiteral("warnings: the guard was not confirmed on a no record"));
        QVERIFY(panel.statusLine().startsWith(QStringLiteral("Needs a look")));

        requestSession(panel, p);
        QVERIFY(!p.upgrade->isEnabled());
        panel.onJobStarted(QStringLiteral("job-2"), QStringLiteral("Recovery OS session"));
        panel.onJobFinished(QStringLiteral("job-2"), false, QStringLiteral("exit 7 at upgrade"));
        QVERIFY(panel.statusLine().startsWith(QStringLiteral("Failed")));

        requestSession(panel, p);
        QVERIFY(!p.upgrade->isEnabled());
        panel.onJobStarted(QStringLiteral("job-3"), QStringLiteral("Recovery OS session"));
        panel.onJobFinished(QStringLiteral("job-3"), true, QStringLiteral("clean"));
        QVERIFY(panel.statusLine().startsWith(QStringLiteral("Done")));

        // Another operation's job, and a session job nothing here requested,
        // are not this panel's
        panel.onJobStarted(QStringLiteral("job-4"), QStringLiteral("BackupRun"));
        QVERIFY(panel.facts().ownJobId.isEmpty());
        panel.onJobStarted(QStringLiteral("job-6"), QStringLiteral("Recovery OS session"));
        QVERIFY(panel.facts().ownJobId.isEmpty());
    }

    void thePanelKnowsItsOwnJobAndItsAttendedness()
    {
        DBusClient client;
        RecoveryPanel panel(&client);
        const RecoveryParts p = recoveryPartsOf(panel);
        QVERIFY(p.ok());
        requestSession(panel, p);
        QVERIFY(!p.upgrade->isEnabled()); // requested, attended
        panel.onJobStarted(QStringLiteral("job-5"), QStringLiteral("Recovery OS session"));
        QCOMPARE(panel.facts().ownJobId, QStringLiteral("job-5"));
        QVERIFY(panel.facts().ownJobAttended);
        panel.onJobFinished(QStringLiteral("job-5"), true, QStringLiteral("clean"));
        QVERIFY(panel.facts().ownJobId.isEmpty());

        // A JobFinished that beats the method reply (a helper refusal) is honoured when jobStarted names the job
        requestSession(panel, p);
        panel.onJobFinished(QStringLiteral("job-8"), false, QStringLiteral("refused: a session job is running"));
        panel.onJobStarted(QStringLiteral("job-8"), QStringLiteral("Recovery OS session"));
        QVERIFY(panel.facts().ownJobId.isEmpty());
        QVERIFY(p.upgrade->isEnabled());
    }
```

- [ ] **Step 2: Build to see them fail**

Run: `cmake --build /tmp/das-8249-s3-build --target gui-smoketest 2>&1 | grep -m2 error`
Expected: `recoverypanel.h: No such file or directory`.

- [ ] **Step 3: Implement the panel** (`recoverypanel.cpp`; the parts that carry the rules are shown in full, layout is plain Qt):

```cpp
#include "recoverypanel.h"
#include "dbusclient.h"

#include <KLocalizedString>
#include <QCheckBox>
#include <QClipboard>
#include <QComboBox>
#include <QDateTime>
#include <QDateTimeEdit>
#include <QGroupBox>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QLabel>
#include <QMessageBox>
#include <QProcess>
#include <QPushButton>
#include <QRadioButton>
#include <QStandardPaths>
#include <QTimer>
#include <QVBoxLayout>

namespace {
const auto Operation = QStringLiteral("Recovery OS session");
constexpr int RefreshMs = 30000;
const auto Banner = QStringLiteral("this OS runs btrbk at boot; the guard is stopping it now; disable it in this session");
} // namespace

RecoveryPanel::RecoveryPanel(DBusClient *client, QWidget *parent)
    : QWidget(parent), m_client(client)
{
    auto *layout = new QVBoxLayout(this);
    m_cards = new QVBoxLayout;
    layout->addLayout(m_cards);
    buildControls();
    layout->addStretch();
    m_status = new QLabel(this);
    m_status->setWordWrap(true);
    layout->addWidget(m_status);

    m_timer = new QTimer(this);
    m_timer->setInterval(RefreshMs);
    connect(m_timer, &QTimer::timeout, this, &RecoveryPanel::refresh);

    connect(m_client, &DBusClient::recoveryOsStatusResult, this, &RecoveryPanel::onStatusResult);
    connect(m_client, &DBusClient::jobStarted, this, &RecoveryPanel::onJobStarted);
    connect(m_client, &DBusClient::jobFinished, this, &RecoveryPanel::onJobFinished);
    connect(m_client, &DBusClient::recoveryOsSessionEndResult, this, &RecoveryPanel::onSessionEndResult);
    connect(m_client, &DBusClient::recoveryOsScheduleResult, this, &RecoveryPanel::onScheduleResult);
    connect(m_client, &DBusClient::recoveryOsConsoleResult, this, &RecoveryPanel::onConsoleResult);
    connect(m_client, &DBusClient::errorOccurred, this, [this](const QString &op, const QString &) {
        if (op == QLatin1String("RecoveryOsStatus"))
            m_refreshInFlight = false;
        if (op == QLatin1String("RecoveryOsSession") || op == QLatin1String("Recovery OS session"))
            m_jobRequested = false;
        if (op == QLatin1String("RecoveryOsScheduleSet") || op == QLatin1String("RecoveryOsConsole"))
            refresh();
        rederive();
    });

    if (!m_client->isAvailable()) {
        m_parseError = m_client->unavailableReason();
        m_status->setText(m_parseError);
    }
    rederive();
}

void RecoveryPanel::buildControls()
{
    auto *row = new QHBoxLayout;
    m_selector = new QComboBox(this);
    m_selector->setObjectName(QStringLiteral("selector"));
    m_selector->setToolTip(i18n("Which recovery drive the buttons act on, or both drives in one run"));
    row->addWidget(m_selector);

    m_attended = new QRadioButton(i18n("Attended"), this);
    m_attended->setObjectName(QStringLiteral("attended"));
    m_attended->setToolTip(i18n("The console opens; you log in and run the checklist. Required for a drive's first session, where you install and enable qemu-guest-agent"));
    m_attended->setChecked(true);
    m_unattended = new QRadioButton(i18n("Unattended"), this);
    m_unattended->setObjectName(QStringLiteral("unattended"));
    m_unattended->setToolTip(i18n("The recovery OS upgrades itself through its guest agent and powers off; needs the agent installed and started at boot, and no 'will' record"));
    row->addWidget(m_attended);
    row->addWidget(m_unattended);

    m_sequential = new QRadioButton(i18n("One after the other"), this);
    m_sequential->setObjectName(QStringLiteral("sequential"));
    m_sequential->setToolTip(i18n("Both drives: the second starts only after the first ended clean — a bad update reaches one drive, not both"));
    m_parallel = new QRadioButton(i18n("In parallel"), this);
    m_parallel->setObjectName(QStringLiteral("parallel"));
    m_parallel->setToolTip(i18n("Both drives at once: halves the time backups wait; the everyday choice once each drive has 3 clean unattended runs"));
    row->addWidget(m_sequential);
    row->addWidget(m_parallel);

    m_now = new QCheckBox(i18n("Now"), this);
    m_now->setObjectName(QStringLiteral("now"));
    m_now->setToolTip(i18n("Start the session now; untick to pick a date and time for an unattended one"));
    m_now->setChecked(true);
    m_when = new QDateTimeEdit(QDateTime::currentDateTime().addSecs(3600), this);
    m_when->setObjectName(QStringLiteral("when"));
    m_when->setCalendarPopup(true);
    m_when->setToolTip(i18n("When the unattended session runs; at least 2 minutes ahead"));
    m_when->setEnabled(false);
    row->addWidget(m_now);
    row->addWidget(m_when);
    static_cast<QVBoxLayout *>(layout())->addLayout(row);

    auto *buttons = new QHBoxLayout;
    auto button = [this, buttons](const char *name, const QString &text, void (RecoveryPanel::*slot)()) {
        auto *b = new QPushButton(text, this);
        b->setObjectName(QLatin1String(name));
        b->setEnabled(false);
        connect(b, &QPushButton::clicked, this, slot);
        buttons->addWidget(b);
        return b;
    };
    m_upgrade = button("upgrade", i18n("Upgrade"), &RecoveryPanel::onUpgrade);
    m_schedule = button("schedule", i18n("Schedule"), &RecoveryPanel::onSchedule);
    m_clear = button("clearSchedule", i18n("Clear schedule"), &RecoveryPanel::onClearSchedule);
    m_console = button("console", i18n("Open console"), &RecoveryPanel::onConsole);
    m_end = button("endSession", i18n("End session"), &RecoveryPanel::onEndSession);
    static_cast<QVBoxLayout *>(layout())->addLayout(buttons);

    connect(m_selector, &QComboBox::currentIndexChanged, this, &RecoveryPanel::rederive);
    connect(m_attended, &QRadioButton::toggled, this, &RecoveryPanel::rederive);
    connect(m_now, &QCheckBox::toggled, this, [this](bool now) { m_when->setEnabled(!now); rederive(); });
    connect(m_when, &QDateTimeEdit::dateTimeChanged, this, &RecoveryPanel::rederive);
}

void RecoveryPanel::applyDocument(const QByteArray &json)
{
    QString err;
    auto doc = RecoveryDocument::parse(json, &err);
    if (!doc) {
        m_doc.reset();
        m_parseError = err;
    } else {
        m_parseError.clear();
        const int keep = m_selector->currentIndex();
        m_doc = std::move(doc);
        m_selector->blockSignals(true);
        m_selector->clear();
        for (const DriveView &d : m_doc->drives)
            m_selector->addItem(d.displayName.isEmpty() ? d.label : d.displayName, d.label);
        if (m_doc->drives.size() == 2)
            m_selector->addItem(i18n("Both drives"), QStringLiteral("both"));
        m_selector->setCurrentIndex(keep >= 0 && keep < m_selector->count() ? keep : 0);
        m_selector->blockSignals(false);
        if (!m_sequential->isChecked() && !m_parallel->isChecked())
            (m_doc->pair.modeDefault == QLatin1String("parallel") ? m_parallel : m_sequential)->setChecked(true);
    }
    rebuildCards();
    rederive();
}

GuiFacts RecoveryPanel::facts() const
{
    GuiFacts g;
    g.ownJobId = m_ownJobId;
    g.ownJobAttended = m_ownJobAttended;
    g.viewerInstalled = !QStandardPaths::findExecutable(QStringLiteral("remote-viewer")).isEmpty();
    g.nowEpoch = QDateTime::currentSecsSinceEpoch();
    g.chosenNow = m_now->isChecked();
    g.chosenEpoch = m_when->dateTime().toSecsSinceEpoch();
    g.unattended = m_unattended->isChecked();
    return g;
}

void RecoveryPanel::rederive()
{
    const bool both = selectionIsBoth();
    m_sequential->setVisible(both);
    m_parallel->setVisible(both);
    auto bind = [](QPushButton *b, const Action &a) { b->setEnabled(a.enabled); b->setToolTip(a.why); };
    if (!m_doc || !m_parseError.isEmpty()) {
        const Action off{false, m_parseError.isEmpty() ? i18n("no status document yet") : m_parseError};
        for (QPushButton *b : {m_upgrade, m_schedule, m_clear, m_console, m_end})
            bind(b, off);
        if (!m_parseError.isEmpty())
            m_status->setText(m_parseError);
        return;
    }
    const GuiFacts g = facts();
    if (both) {
        const PairActions p = derivePairActions(*m_doc, g);
        bind(m_upgrade, p.upgrade);
        bind(m_schedule, p.schedule);
        bind(m_clear, p.clearSchedule);
        bind(m_console, {false, i18n("pick one drive to open its console")});
        bind(m_end, {false, i18n("pick one drive to end its session")});
    } else if (DriveView *d = selectedDrive()) {
        const DriveActions a = deriveActions(*d, m_doc->pair, g);
        bind(m_upgrade, a.upgrade);
        bind(m_schedule, a.schedule);
        bind(m_clear, a.clearSchedule);
        bind(m_console, a.console);
        bind(m_end, a.endSession);
    }
    if (m_jobRequested || !m_ownJobId.isEmpty()) {
        m_upgrade->setEnabled(false);
        m_upgrade->setToolTip(i18n("this window's session is running"));
    }
    if (m_endInFlight) {
        m_end->setEnabled(false);
        m_end->setText(i18n("Ending…"));
    } else {
        m_end->setText(i18n("End session"));
    }
}
```

`rebuildCards()` deletes the old cards and, per drive, adds a `QGroupBox` titled `displayName (serials)` holding one `QLabel` of lines — OS, installed, last full upgrade `+ ageWords`, kernel vs host kernel, btrfs-progs vs host, btrbk (or "package database not read" when `!packagesRead`), current/**STALE** with `assessment->reasons`, `verdictWords(verdict)` with the record's reasons, record age `ageWords(checkedEpoch)`, clean runs (`cleanRuns` or `cleanRunsError`), schedule (`scheduleWords` or `scheduleError`), session (`sessionWords` or `sessionError`), and **UPDATE DUE** when `due` — and a second `QLabel` (objectName `errors`) listing every `*_error` as rich text in the scheme's negative colour: `KColorScheme(QPalette::Active).foreground(KColorScheme::NegativeText).color().name()` inside `<span style='color:…'>`, with `#include <KColorScheme>` and `KF6::ColorScheme` linked in both CMake targets (add `ColorScheme` to the `find_package(KF6 …)` component list; read that list first). A pair card follows when there are two drives (pair schedule, pair session, mode default).

The click slots:

```cpp
void RecoveryPanel::onUpgrade()
{
    const QStringList labels = selectedLabels();
    const bool unattended = m_unattended->isChecked();
    if (!unattended && !selectionIsBoth()) {
        if (DriveView *d = selectedDrive(); d && d->verdict == QLatin1String("will")) {
            const auto answer = QMessageBox::warning(this, i18n("This recovery OS runs btrbk at boot"),
                i18n("%1\n\nThe session boots it under the guard. At the console, disable what the record's reasons name before you power off. Continue?", Banner),
                QMessageBox::Yes | QMessageBox::No, QMessageBox::No);
            if (answer != QMessageBox::Yes)
                return;
        }
    }
    const QString mode = selectionIsBoth() ? (m_parallel->isChecked() ? QStringLiteral("parallel") : QStringLiteral("sequential")) : QString();
    m_jobRequested = true;
    m_ownJobAttended = !unattended;
    rederive();
    m_client->recoveryOsSession(labels, unattended, mode);
}

void RecoveryPanel::onSchedule()
{
    const QStringList labels = selectedLabels();
    const qint64 at = m_when->dateTime().toSecsSinceEpoch();
    const std::optional<ScheduleView> existing = selectionIsBoth() ? m_doc->pair.schedule : selectedDrive()->schedule;
    if (existing && (existing->state == QLatin1String("pending") || existing->state == QLatin1String("missed"))) {
        const QString when = existing->atEpoch ? QDateTime::fromSecsSinceEpoch(*existing->atEpoch).toString(Qt::TextDate) : i18n("a time that cannot be read");
        if (QMessageBox::question(this, i18n("Replace the schedule?"),
                i18n("%1 is already scheduled for %2. Replace it?", existing->unit, when)) != QMessageBox::Yes)
            return;
    }
    const QString mode = selectionIsBoth() ? (m_parallel->isChecked() ? QStringLiteral("parallel") : QStringLiteral("sequential")) : QString();
    m_client->recoveryOsScheduleSet(labels, at, mode);
}

void RecoveryPanel::onClearSchedule()
{
    m_client->recoveryOsScheduleSet(selectedLabels(), 0, QString());
}

void RecoveryPanel::onConsole()
{
    m_client->recoveryOsConsole(selectedLabels().first());
}

void RecoveryPanel::onConsoleResult(const QString &label, const QString &path)
{
    const auto [program, args] = DBusClient::consoleCommand(path);
    if (!QStandardPaths::findExecutable(program).isEmpty() && QProcess::startDetached(program, args)) {
        m_status->setText(i18n("Console of %1 opened in %2", label, program));
        return;
    }
    QMessageBox box(QMessageBox::Information, i18n("Console socket"),
        i18n("%1 is not installed (package virt-viewer). Connect any VNC viewer that speaks UNIX sockets to:\n\n%2\n\nThe socket accepts one connection and is removed when the drive is given back.", program, path),
        QMessageBox::Ok, this);
    QPushButton *copy = box.addButton(i18n("Copy path"), QMessageBox::ActionRole);
    copy->setToolTip(i18n("Copy the socket's path to the clipboard"));
    box.exec();
    if (box.clickedButton() == copy)
        QGuiApplication::clipboard()->setText(path);
}

void RecoveryPanel::onEndSession()
{
    m_endInFlight = true;
    rederive();
    m_client->recoveryOsSessionEnd(selectedLabels().first());
}

void RecoveryPanel::onSessionEndResult(const QString &label, bool ok, const QString &lines)
{
    m_endInFlight = false;
    m_status->setText(ok ? i18n("Session of %1 ended:\n%2", label, lines) : i18n("Ending the session of %1 failed:\n%2", label, lines));
    refresh();
}

void RecoveryPanel::onScheduleResult(const QString &unit)
{
    m_status->setText(i18n("Schedule written: %1", unit));
    refresh();
}

void RecoveryPanel::onJobStarted(const QString &jobId, const QString &operation)
{
    if (operation != Operation)
        return;
    if (m_earlyFinished.remove(jobId)) { // finished before its reply (a helper refusal)
        m_jobRequested = false;
        rederive();
        return;
    }
    if (!m_jobRequested)
        return; // another window's session: the document will show it
    m_jobRequested = false;
    m_ownJobId = jobId;
    rederive();
}

void RecoveryPanel::onJobFinished(const QString &jobId, bool success, const QString &summary)
{
    if (m_ownJobId.isEmpty() && m_jobRequested) {
        m_earlyFinished.insert(jobId);
    }
    if (jobId != m_ownJobId)
        return;
    m_ownJobId.clear();
    if (success)
        m_status->setText(i18n("Done: %1", summary));
    else if (summary.startsWith(QLatin1String("warnings:")))
        m_status->setText(i18n("Needs a look: %1", summary));
    else
        m_status->setText(i18n("Failed: %1", summary));
    rederive();
    refresh();
}

void RecoveryPanel::refresh()
{
    if (m_refreshInFlight)
        return;
    m_refreshInFlight = true;
    m_client->recoveryOsStatusAsync();
}

void RecoveryPanel::onStatusResult(const QString &json)
{
    m_refreshInFlight = false;
    if (json.isEmpty())
        return; // the error was reported by the client
    applyDocument(json.toUtf8());
}

void RecoveryPanel::setShown(bool shown)
{
    if (shown) { refresh(); m_timer->start(); } else { m_timer->stop(); }
}

QString RecoveryPanel::statusLine() const { return m_status->text(); }
```

The panel adopts a `jobStarted` only after its own click set `m_jobRequested` (another window's session is shown by the document, never adopted). Both job tests therefore request a session first through this helper, placed with the other static helpers of the test class; `aWarnedSessionNeedsALookAFailedOneFailed` calls `requestSession(panel, p)` before each `onJobStarted`, and `thePanelKnowsItsOwnJobAndItsAttendedness` replaces its inline idling of drive A with it:

```cpp
    // The fixture's drive A with its running job removed, then Upgrade
    // clicked: the panel now awaits jobStarted for a session of its own.
    static void requestSession(RecoveryPanel &panel, const RecoveryParts &p)
    {
        QJsonDocument d = QJsonDocument::fromJson(fixture());
        QJsonObject o = d.object();
        QJsonArray drives = o[QStringLiteral("drives")].toArray();
        QJsonObject a = drives[0].toObject();
        a[QStringLiteral("session")] = QJsonValue::Null;
        drives[0] = a;
        o[QStringLiteral("drives")] = drives;
        panel.applyDocument(QJsonDocument(o).toJson());
        p.selector->setCurrentIndex(0);
        p.attended->setChecked(true);
        QVERIFY(p.upgrade->isEnabled());
        p.upgrade->click(); // verdict "may": no banner dialog
        QVERIFY(!p.upgrade->isEnabled());
    }
```

(`QVERIFY` inside a static helper returns from the helper only; follow each call with `QVERIFY(!p.upgrade->isEnabled());` in the test body so a failed request fails the test.) The smoke test needs `#include <QJsonArray>`, `<QJsonDocument>`, `<QJsonObject>`, `<QComboBox>`, `<QDateTimeEdit>` beside its existing includes.

Sidebar: add `RecoveryDrives,` after `HealthStatus` in the enum; in `buildTree`, after the Health section:

```cpp
    // Recovery drives (leaf)
    auto *recovery = new QTreeWidgetItem(this);
    recovery->setText(0, tr("Recovery drives"));
    recovery->setIcon(0, QIcon::fromTheme(QStringLiteral("system-reboot")));
    recovery->setToolTip(0, tr("Each recovery drive's CachyOS: its status, and updating it in a VM"));
    recovery->setData(0, SectionRole, static_cast<int>(SidebarSection::RecoveryDrives));
```

MainWindow: `class RecoveryPanel;` and `RecoveryPanel *m_recoveryPanel = nullptr;` in the header; after page 4: `m_recoveryPanel = new RecoveryPanel(m_dbusClient, this); m_stack->addWidget(m_recoveryPanel); // index 5`; in `onSectionChanged` every existing case gains nothing, and a new case:

```cpp
    case SidebarSection::RecoveryDrives:
        m_stack->setCurrentIndex(5);
        m_recoveryPanel->setShown(true);
        break;
```

and at the top of `onSectionChanged`: `if (m_recoveryPanel && section != SidebarSection::RecoveryDrives) m_recoveryPanel->setShown(false);`.

CMake: `src/recoverypanel.cpp` in both source lists; `KF6::ColorScheme` in both link lists (and `ColorScheme` in the `find_package(KF6 … COMPONENTS …)` list — read it).

- [ ] **Step 4: Build and run the suite**

Run: `cmake --build /tmp/das-8249-s3-build 2>&1 | grep -E 'error|warning' ; ctest --test-dir /tmp/das-8249-s3-build -R gui-smoketest --output-on-failure 2>&1 | grep -E 'FAIL|Totals'`
Expected: no compiler output (`-Werror`), `0 failed`.

- [ ] **Step 5: Launch the GUI against the dead bus and look** — `QT_QPA_PLATFORM=offscreen` cannot show it; run `DBUS_SYSTEM_BUS_ADDRESS=unix:path=/nonexistent /tmp/das-8249-s3-build/bin/btrdasd-gui` on the desktop, open Recovery drives, screenshot (`spectacle -b -o /tmp/das-8249-s3-recovery-unavailable.png`), confirm: the section exists, every button disabled with the unavailable reason as tooltip, the status line says why. Keep the screenshot for the review. (The live rendering with a document is Task 8.)

- [ ] **Step 6: Counter-test** — in `onJobFinished` swap the `warnings:` branch to say `Failed`: `aWarnedSessionNeedsALookAFailedOneFailed` FAILs; restore.

- [ ] **Step 7: Commit**

```bash
git add gui/src/recoverypanel.h gui/src/recoverypanel.cpp gui/src/sidebar.h gui/src/sidebar.cpp gui/src/mainwindow.h gui/src/mainwindow.cpp gui/CMakeLists.txt gui/tests/smoketest.cpp
git commit -S -m "$(cat <<'EOF'
gui: the Recovery drives section

One card per drive from the status document, a selector (A, B, both),
attended/unattended, sequential/parallel for both, Now or a time, and
Upgrade / Schedule / Clear schedule / Open console / End session, each
bound to deriveActions' {enabled, why}. The will banner is a
confirmation before the call; exit 5 reads "Needs a look"; the console
opens in remote-viewer or shows the socket path. Refresh on entry,
every 30 s while shown, and on the job signals.
EOF
)"
```

---

### Task 7: packaging, docs, changelog, and the whole-branch checks

**Files:**
- Modify: `packaging/debian/control` (line 21), `packaging/fedora/das-backup-manager.spec` (the GUI subpackage's `Requires` block ~line 21), `packaging/flatpak/org.theboscoclub.btrdasd-gui.yml`, `packaging/snap/snapcraft.yaml`
- Modify: `docs/ARCHITECTURE.md` (the GUI component table ~line 660, the tests table, the "GUI Read Path" diagram ~line 294), `docs/DISASTER-RECOVERY-GUIDE.md` (new `#### From the GUI` after `#### Scheduled sessions` ~line 1366), `CHANGELOG.md`

- [ ] **Step 1: Packaging** — Debian `Recommends: s-nail, virt-viewer`; Fedora GUI subpackage `Recommends:     virt-viewer`; Flatpak and Snap: a comment next to the GUI app entry: `# The recovery-drive console opens remote-viewer on the host; inside this sandbox the GUI shows the socket's path instead (8249 stage 3).` Verify `packaging/arch/PKGBUILD` already lists `virt-viewer` in `optdepends` (line 19) and leave it.

- [ ] **Step 2: Docs** — ARCHITECTURE: the component table gains `RecoveryStatus | recoverystatus.h/cpp | Pure model of RecoveryOsStatus's document and the panel's enablement rules; unit-tested in gui-smoketest against gui/tests/fixtures/recovery-status.json, which a panel.rs test writes` and `RecoveryPanel | recoverypanel.h/cpp | Recovery drives: cards, selector, Upgrade/Schedule/Clear/Console/End; remote-viewer on the console socket`; "20 C++ components" → 22; the Sidebar row names the section; the GUI Read Path diagram gains `RecoveryOsStatus() → RecoveryPanel`; the tests table's GUI row count from `ctest --test-dir /tmp/das-8249-s3-build -R gui-smoketest -V | grep -c 'PASS   :'`. DISASTER-RECOVERY-GUIDE `#### From the GUI`: one paragraph per button in the spec's §4 words, the banner, "Needs a look", where the console path dialog comes from, that End session is for a holder a crashed driver left and that a running job is cancelled from the progress panel.

- [ ] **Step 3: CHANGELOG** under `## [Unreleased]`:

`### Added`:
```markdown
- **Recovery drives in the GUI** (bd `DAS-Backup-Manager-8249`, stage 3) — a sidebar section with one card per recovery drive from the helper's status document (OS, installed, last full upgrade and age, kernel and btrfs-progs against the host, btrbk, current or STALE with the reasons, the boot verdict, the record's age, clean runs, schedule, session, an UPDATE DUE marker), a selector (one drive, or both), attended or unattended, one after the other or in parallel for both, Now or a date and time, and Upgrade, Schedule, Clear schedule, Open console and End session — every button's tooltip says what it does or why it is disabled, the cautious side first; a `will` record shows the banner before an attended session; a session that ended with warnings reads "Needs a look"; the console opens in `remote-viewer` on the helper's private socket, or the socket's path is shown to copy when virt-viewer is not installed. `RecoveryOsSessionEnd` is called with a 10-minute timeout (bd `DAS-Backup-Manager-k84b`). The document is pinned by `gui/tests/fixtures/recovery-status.json`, written by a `panel.rs` test and parsed by `gui-smoketest`
```
`### Changed`: `- **Debian and Fedora packages recommend virt-viewer** for the recovery-drive console (Arch's `optdepends` already named it)`.
`### Fixed` (beside Task 1's entry):
```markdown
- **The pair's schedule finds its firing beyond the 20-line history cut, and the earliest line after the trigger** (bd `DAS-Backup-Manager-6obo`) — a `both` schedule whose firing was older than 20 sessions of each drive read refused or missed, and a firing without exec readings of its own took a later Now session's line
```

- [ ] **Step 4: Whole-branch checks**

```bash
cd indexer && CARGO_TARGET_DIR=/tmp/das-8249-s3-target cargo test 2>&1 | grep -E '^test result' ; CARGO_TARGET_DIR=/tmp/das-8249-s3-target cargo test --features dbus 2>&1 | grep -E '^test result'; cargo fmt --check; cargo clippy --all-targets -- -D warnings; cargo clippy --all-targets --features dbus -- -D warnings; cd ..
ctest --test-dir /tmp/das-8249-s3-build --output-on-failure 2>&1 | tail -4
shellcheck scripts/*.sh tests/*.sh; shfmt -d -i 4 -ci scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh
codespell docs CHANGELOG.md gui/src gui/tests packaging
# mutation gate on a copy
rm -rf /tmp/das-8249-s3-mut && cp -r /tmp/das-8249-s3 /tmp/das-8249-s3-mut && cd /tmp/das-8249-s3-mut/indexer && git diff --relative origin/main -- . > mutants.diff && PATH=/tmp/mailshim:$PATH env -u CARGO_TARGET_DIR cargo mutants --in-place --no-shuffle --in-diff mutants.diff; env -u CARGO_TARGET_DIR python3 ../.github/scripts/mutants-gate.py
~/.claude/bin/scrub-promo check origin/main..HEAD
```

Expected: every `test result: ok`; ctest `100% tests passed`; lint silent; `mutants OK`; `CLEAN`. Record each line in the final report.

- [ ] **Step 5: Commit, push the branch, open the PR for the fresh reviewer**

```bash
git add packaging docs CHANGELOG.md
git commit -S -m "$(cat <<'EOF'
8249 stage 3: packaging, docs and changelog for the Recovery drives section
EOF
)"
git push -u origin 8249-stage3
```

Tracked follow-ups to name in the report: bd `epmw` (orphan schedule units), `vrij`, `1ley`, `oz7w`, `f37n`, `gbgj`; `k84b` closed by this branch (`bd close k84b --reason="GUI calls RecoveryOsSessionEnd with a 10-minute timeout (8249 stage 3)"`), `c8lf` and `6obo` closed by Tasks 1 and 2 after merge.

---

### Task 8: install on the host and the three real-VM proofs (after merge, main checkout, operator present)

**Files:** none in the tree; outputs go into bd `8249` notes and the session record.

**Preconditions, in this order, each with its output kept:**

```bash
cd /hddRaid1/ClaudeCodeProjects/DAS-Backup-Manager && git status --short && git log --oneline -1   # clean, the merge commit
busy=; for u in das-backup.service das-backup-full.service das-scrub.service das-backup-doctor.service; do case "$(systemctl show -P ActiveState "$u")" in inactive | failed) ;; *) busy=1 ;; esac; done; flock -n /run/das-backup.lock true || busy=1; [ -z "$busy" ] || echo "WAIT — do not install"
cmake -B build -DCMAKE_BUILD_TYPE=Release && cmake --build build
```

- [ ] **Step 1: Install** (only with no `WAIT` above):

```bash
sudo cmake --install build
sudo btrdasd setup --upgrade
sudo /usr/lib/das-backup/recovery-os-vm.sh define
sudo btrdasd setup --check
busctl introspect org.dasbackup.Helper1 /org/dasbackup/Helper1 | grep -c RecoveryOs   # 5
busctl introspect org.dasbackup.Helper1 /org/dasbackup/Helper1 | grep RecoveryOsScheduleSet   # asxs → s
sudo btrdasd recovery-os status --state-file && sudo jq .schema_version /var/lib/das-backup/recovery-os.json   # 4
```

Expected: `setup --check` clean; 5 methods; `ScheduleSet` takes `asxs`; the record is schema 4 (if `--state-file` needs the drives attached and they are not, the next nightly writes it — note which).

- [ ] **Step 2: The panel against the live helper** — `btrdasd-gui`, Recovery drives: both cards populated from the record, every tooltip read by hovering each button in both states (screenshot `spectacle -b -o /tmp/8249-s3-live.png`). With the real drives attached and idle, **do not click Upgrade**: the proofs run on copies.

- [ ] **Step 3: The loop-hatch rig** — repeat stage 1's proof setup from bd `8249`'s note of 2026-10-07 15:40 (`.superpowers/sdd/handoff-2026-10-05/` and the stage 1 session record name the exact commands: a raw copy of `test-vm-cachyos`, the loop device, the planted record via the real binary, `DAS_RECOVERY_VM_TEST_ROOT`-free — the helper runs the installed script with a clean environment, so the rig must present the copy as a real `ata-*_<serial>` disk: read how stage 1 did it before deciding; if it used the script's test seams, the proof must instead run the script by hand with those seams for the two-drive and banner cases and the GUI for the console case, and the report must say so). Two copies for the two-drive proof.

- [ ] **Step 4: Proof 1 — console socket, from the GUI.** Attended Upgrade on the rig's drive A → the Progress dock shows the session's lines → Open console → remote-viewer shows the recovery OS's login prompt. Counter-tests: `ls -la /run/das-recovery-os-vm/` (the socket 0600 your uid, the directory 0711 root); a second `remote-viewer` on the same path is refused; after the VM powers off and the give-back runs, the socket is gone. Record the three outputs.

- [ ] **Step 5: Proof 2 — two-drive run, from the GUI.** Both drives, unattended, sequential: the dock shows A then B, `DRIVE … 0` twice, the pair session was shown in the panel while it ran (screenshot), `sudo /usr/lib/das-backup/recovery-os-vm.sh history` shows two new lines with `mode: sequential`. Counter-test: plant drive A's `upgrade` to fail on the rig (stage 1 planted `echo 3 >"$S/stage.upgrade.rc"` in the stub suite; on the real rig, make pacman fail inside copy A — remove its network route, or mask `pacman` by a `chmod 000 /usr/bin/pacman` inside the copy's `@` before the run) → B `skipped`, the cause named in the dock's closing lines and in the panel's "Failed:" status. Then parallel on healthy copies: both domains `running` at once (`virsh list`), two history lines `mode: parallel`. Cancel during a parallel run from the dock: both `DRIVE … 3`, both VMs still running, the lock held, End session on each gives them back (Task 1's proof on real domains).

- [ ] **Step 6: Proof 3 — the attended `will` banner.** Plant a `will` verdict for rig drive A (stage 1's planted record, or a copy whose `btrbk.timer` is enabled, then `recovery-os status --state-file`) → the panel shows `will run btrbk at boot` and Upgrade enabled (attended) → click → the banner dialog → Yes → the dock's lines show the banner before the boot and again when the guard confirms → at the console, `systemctl status btrbk.timer` is masked. Counter-tests: Unattended radio → Upgrade disabled with the record's `why`; `busctl call … RecoveryOsSession 'asbsb' 1 <label> true "" false` → the helper's `InvalidArgs` refusal.

- [ ] **Step 7: Tear down and record.** Domains undefined with `--nvram` for the rig, loops detached, copies removed, `ip rule show | grep -c 5100` → 0, `/run/das-maintenance.lock` free, `/run/das-recovery-os-vm` empty. Append the outputs of Steps 1–6 to bd `8249` (`--append-notes`) and the session record; close `k84b`, `c8lf`, `6obo`.
