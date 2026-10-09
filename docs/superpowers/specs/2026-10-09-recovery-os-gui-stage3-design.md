# Recovery drives in the GUI — 8249 stage 3

- **Status:** design; approved by the operator section by section 2026-10-09
- **Date:** 2026-10-09
- **Applies to:** `btrdasd-gui` 0.7.23.x → next minor; helper unchanged except where §8 says
- **Tracker:** bd `DAS-Backup-Manager-8249` (stage 3 of 3)
- **Builds on:** stage 1 (`recovery-os-vm.sh session`, merged 21aef94) and stage 2 (the
  helper's five `RecoveryOs*` methods and the status document, merged 090ab5c;
  plan `docs/superpowers/plans/2026-10-08-recovery-os-helper-stage2.md`)

## 1. What stage 3 delivers

The operator's request of 2026-10-04: a section of `btrdasd-gui` showing each recovery
drive's CachyOS status, a drive selector, and an Upgrade button; attended sessions open the
console; unattended ones run now or at a chosen time, per drive or both drives, sequential by
default until three clean runs. Stage 2 built everything below the GUI. Stage 3 is:

1. The panel and its pure model (§3, §4).
2. Five `DBusClient` wrappers (§3.3).
3. The console viewer launch (§3.5).
4. The host install of stages 1–3, which have never been installed (`/var/lib/das-backup/recovery-os.json` is still schema 3) — §7.
5. The three real-VM proofs stage 1 left owed: console socket, a two-drive run, the attended `will` banner — §7.
6. Two carried defects fixed, one closed by design, the rest tracked — §8.

## 2. Decisions taken at the design review (2026-10-09)

| # | Question | Decision |
|---|---|---|
| 1 | `RecoveryOsStatus` sits behind `org.dasbackup.health` (any active seat) and carries the schedule units' last journal lines and the session history. | **Acceptable as-is.** The lines are the script's own refusal and outcome words; the history holds labels, dates, exit codes and a kernel version — nothing beyond what Health already shows. Keeping `health` means the panel loads without a password prompt. |
| 2 | `RecoveryOsSessionEnd` is synchronous and may exceed Qt's 25 s default (bd `k84b`). | **The GUI calls it asynchronously with a 10-minute timeout.** No helper change; the give-back has its own bound inside the script. |
| 3 | Which viewer opens the attended console. | **`remote-viewer vnc+unix://<path>`** (virt-viewer). Optional runtime dependency in every packaging format; when absent the panel shows the path to copy. krdc rejected: its VNC backend takes host:port, so a UNIX socket would need a loopback TCP bridge, which widens who can connect. |
| 4 | Where the enablement rules live. | **A pure model and a thin widget** (approach B, §3.1). Rules are pinned headlessly both ways; the helper is untouched. |
| 5 | How a `session_error` renders. | **A blocker, not a tooltip only:** the error is shown in the session's place and every action depending on that part is disabled with the error as its tooltip (§5). |

## 3. Components

### 3.1 `gui/src/recoverystatus.{h,cpp}` — the pure model (QtCore only, no widgets)

- `struct RecoveryDocument { int schema; int maxAgeDays; QString today; PairView pair; QList<DriveView> drives; }`
  with `static std::optional<RecoveryDocument> parse(const QByteArray &json, QString *error)`.
  Every key of stage 2's `status_json` has a field; every `*_error` key is a `std::optional<QString>`.
  A document whose `schema` is not 1 is a parse error naming the number.
- `struct DriveView`: `label`, `displayName`, `serials`, `checkedEpoch`, `recordError`,
  `record` (`os`, `installed`, `lastFullUpgrade`, `kernel`, `hostKernel`, `btrfsProgs`,
  `hostBtrfsProgs`, `btrbk`, `packagesRead`, `guestAgent{state, installed, enabled, why}`),
  `assessment` (as the document has it: current/stale, age, reasons), `due`, `verdict`
  (`will`|`may`|`no`), `unattended{possible, why}`, `cleanRuns`, `cleanRunsError`, `history`
  (lines as parsed objects), `historyError`, `schedule{unit, atEpoch, mode, state, detail}`,
  `scheduleError`, `session{by, sinceEpoch, domainState, attended}`, `sessionError`.
- `struct PairView`: `modeDefault`, `schedule`, `scheduleError`, `session`, `sessionError`.
- `struct GuiFacts { QString ownJobId; bool viewerInstalled; qint64 nowEpoch; qint64 chosenEpoch; bool chosenNow; bool unattended; QString mode; }`
  — what only the window knows.
- `struct Action { bool enabled; QString why; }` — `why` is the tooltip in both states (what
  the button does when enabled, why not when disabled).
- `struct DriveActions { Action upgradeAttended, upgradeUnattended, schedule, clearSchedule, console, endSession; bool bannerNeeded; }`
  and `DriveActions deriveActions(const DriveView &, const PairView &, const GuiFacts &)`.
- `struct PairActions { Action upgrade, schedule, clearSchedule; QString modeDefault; }`
  and `PairActions derivePairActions(const RecoveryDocument &, const GuiFacts &)`.
- Formatting helpers the widget reads: `ageWords(epoch, now)`, `verdictWords(verdict)`,
  `sessionWords(session)`, `scheduleWords(schedule)`. Each pinned by the smoke test.

### 3.2 `gui/src/recoverypanel.{h,cpp}` — the thin widget

- One card (a `QGroupBox`) per drive in document order: display name and serials; OS,
  installed, last full upgrade and its age, kernel vs host kernel, btrfs-progs vs host,
  btrbk, `packages_read` false shown as "package database not read"; current / **STALE**
  with the assessment's reasons; the boot verdict with its reasons; record age; clean
  runs; schedule (state, time, mode, detail); session (by, since, domain state,
  attended); an **update due** marker when `due`.
- A selector: drive A / drive B / both (labels from the document, in its order).
- Attended / unattended radios; sequential / parallel radios shown for **both**, default
  `pair.mode_default`, the operator may pick either.
- A `QDateTimeEdit` with a **Now** checkbox; the edit is disabled while Now is ticked.
- Buttons: **Upgrade**, **Schedule**, **Clear schedule**, **Open console**, **End session**.
  Every button carries the `Action.why` tooltip from §3.1 (the project's tooltip rule).
- Refresh: on entering the section, every 30 s while the section is shown (the timer stops
  when it is not), and on every `jobStarted` / `jobFinished`. A refresh in flight is not
  re-issued.
- Session output: the existing Progress dock renders the job's `JobProgress` / `JobLog`
  lines and owns Cancel; the panel only re-derives on the job signals. The dock's Cancel
  tooltip gains the parallel caveat while bd `c8lf` stands (removed when §8 fixes it).

### 3.3 `DBusClient` — five wrappers

| Wrapper | Helper method | Shape |
|---|---|---|
| `recoveryOsStatusAsync()` | `RecoveryOsStatus` | async; signal `recoveryOsStatusResult(QString json)` |
| `recoveryOsSession(labels, unattended, mode, acceptRisk=false)` | `RecoveryOsSession` | a job via `callAsync`, operation "Recovery OS session"; `jobStarted` names the id |
| `recoveryOsSessionEnd(label)` | `RecoveryOsSessionEnd` | async on an interface whose timeout is **600 000 ms**; signal `recoveryOsSessionEndResult(bool ok, QString lines)` |
| `recoveryOsScheduleSet(labels, atEpoch, mode)` | `RecoveryOsScheduleSet` | async; signal `recoveryOsScheduleResult(QString unit)`; `atEpoch == 0` clears |
| `recoveryOsConsole(label)` | `RecoveryOsConsole` | async; signal `recoveryOsConsoleResult(QString path)` |

Errors from any of them go through the existing `errorOccurred(operation, text)` and
`mapDBusError`. The GUI **never** passes `accept_boot_record_risk = true`.

### 3.4 Sidebar and main window

A new top-level sidebar entry **Recovery drives** (`SidebarSection::RecoveryDrives`), stack
index 5, constructed like the other pages with the shared `DBusClient`; entering it calls
`refresh()`.

### 3.5 Console launch

On **Open console**: `recoveryOsConsole(label)`; on the path, `QProcess::startDetached("remote-viewer", {"vnc+unix://" + path})`.
`viewerInstalled` is `QStandardPaths::findExecutable("remote-viewer")` non-empty, probed at
construction and again on each click. When absent, a dialog shows the path with a Copy
button and the words "install virt-viewer, or connect any VNC viewer to this socket". The
path is fetched on every click, never cached (the helper creates it for one connection).

## 4. Data flow and enablement rules

Enter panel → `RecoveryOsStatus` → `parse` → `deriveActions` → bind. A click → one helper
call → on its reply (or error) → refresh. Every rule yields a tooltip; the cautious side wins.

Per drive (`D`), with `P` the pair:

| Action | Enabled when | Disabled tooltip |
|---|---|---|
| Upgrade, attended Now | `D.record` present; no `D.session` and no `P.session`; no `D.sessionError` / `P.sessionError`; no session job or scheduled unit running (the document's session `by` says `job:`/`unit:` when one is); `ownJobId` empty | the first failing condition's words (`recordError`, `sessionError`, "a session holds drive A since …", "this window's session is running") |
| Upgrade, unattended Now | attended rules **and** `D.unattended.possible` | `D.unattended.why` |
| `bannerNeeded` | attended **and** `D.verdict == will` | — (the confirmation dialog shows: *this OS runs btrbk at boot; the guard is stopping it now; disable it in this session*) |
| Schedule | unattended rules, except that a running session does not block scheduling; `chosenNow` false; `chosenEpoch ≥ nowEpoch + 120` (`SCHEDULE_MIN_LEAD`); no `D.scheduleError` | `unattended.why`, "pick a time at least 2 minutes ahead", `scheduleError` |
| Schedule, replacing | an existing `D.schedule` in `pending` or `missed`: the call is made only after a confirmation naming the unit and its time | — |
| Clear schedule | `D.schedule.state` is `pending` or `missed` | "no schedule", or `scheduleError` |
| Open console | `D.session.attended == true` and `D.session.domainState == "running"`; no `sessionError` | "no attended session is running", `sessionError`; when the viewer is missing the button stays enabled and the click shows the path dialog |
| End session | `D.session` present, `D.session.by` begins with `other:` (a holder that is neither a helper job nor a scheduled unit — a driver that crashed); no `sessionError` | "no session to end", "the session is a running job — use Cancel", "a scheduled session is running", `sessionError` |

Pair (`both`): Upgrade and Schedule need both drives' corresponding action enabled and
neither pair error set; `mode` is the chosen radio (`sequential` by default from
`P.modeDefault`); Clear schedule follows `P.schedule`.

Cancel: the Progress dock's Cancel, unchanged (`JobCancel` sends the script one SIGINT).

**Update due**: the document's `due` is shown as the marker; the GUI derives nothing from
`max_age_days` itself.

A session job's `JobFinished(false, "warnings: …")` (exit 5) is shown as **needs a look**
with the summary, distinct from a failure (`false` with any other summary) — the rule stage 2
fixed for exactly this reader.

## 5. Error handling

- Each `*_error` key (`record`, `clean_runs`, `history`, `schedule`, `session`; the pair's
  `schedule` and `session`) is rendered in that part's place in the palette's warning
  colour, and every action depending on that part is disabled with the error as its tooltip
  (§4). A `session_error` therefore blocks Upgrade, Open console and End session for that
  drive.
- A document that cannot be parsed (not JSON, wrong `schema`, a missing key) replaces the
  cards with one line naming the problem and disables every button; the refresh timer keeps
  running.
- Helper refusals (`InvalidArgs`, `Failed`) surface through `errorOccurred` into the existing
  error box; the panel refreshes afterwards so the document, not the GUI's guess, says what
  is now true.
- An unavailable helper: the panel constructs, shows the shared `helperUnavailable` state,
  and every button is disabled with that reason.
- `RecoveryOsSessionEnd` in flight: the button reads **Ending…** and is disabled until the
  reply; a timeout (after 10 minutes) is shown as an error and a refresh follows, since the
  end may still have completed.

## 6. Testing

Headless, in `gui/tests/smoketest.cpp` under `QT_QPA_PLATFORM=offscreen`, every rule both
ways (a document that enables, a document that refuses):

- **Contract fixture** `gui/tests/fixtures/recovery-status.json`: a full document (two
  drives, a pair, a schedule, a session, one of each `*_error` across the two drives). A
  **Rust test** in `panel.rs` loads the same file (path relative to the crate) and asserts
  its key set, recursively, equals `status_json`'s for an equivalent scripted `PanelReads` —
  so a key added on either side fails on both. The fixture is the one source of the contract.
- `parse` of the fixture: every field lands; a wrong `schema` and a missing key are errors
  naming them.
- `deriveActions` / `derivePairActions`: one test per row of §4's table, with the enabling
  document and the refusing one; `bannerNeeded` true only for `will` attended.
- Formatting helpers: `ageWords`, `verdictWords`, `sessionWords`, `scheduleWords`.
- The panel constructs with no helper (added to `panelsConstructWhenTheHelperIsUnavailable`).
- The session-end interface's timeout read back as 600 000 ms.
- The console launch argument: `vnc+unix://` + path, pinned by a seam (`consoleCommand(path)`
  returns program and arguments; the click calls it).
- Existing suites: Rust `cargo test --features dbus`, the script suite, `ctest` (shell and
  GUI steps), and the mutation gate on the Rust diff (the contract test).

## 7. Install and real-VM proofs

Install, in this order and only after the busy check from `.claude/rules/backup.md`
(no `das-*` unit active, `/run/das-backup.lock` free):

1. `cmake --build build && sudo cmake --install build`
2. `sudo btrdasd setup --upgrade` (writes the polkit action, the udev rule, the units)
3. `sudo /usr/lib/das-backup/recovery-os-vm.sh define` (the per-drive domains)
4. `sudo btrdasd setup --check`; `busctl introspect org.dasbackup.Helper1 /org/dasbackup/Helper1 | grep RecoveryOs` shows five methods
5. `btrdasd-gui` → Recovery drives shows both drives' records once the next nightly (or
   `btrdasd recovery-os status --state-file`) has written a schema 4 record.

Proofs owed since stage 1, driven **from the GUI**, on loop-hatch raw copies of
`test-vm-cachyos` as stage 1's proof was (never the real recovery drives):

| Proof | Observed success | Observed failure (counter-test) |
|---|---|---|
| Console socket | Open console launches remote-viewer on the socket; the recovery OS's login prompt is visible; the socket is gone after give-back | a second connection to the same socket is refused; the socket is 0600 the operator's uid, under a 0711 root directory |
| Two-drive run | `both`, sequential: drive 1 then drive 2, one maintenance lock, two history lines, the pair session shown while running; parallel: both domains running at once | sequential with drive 1 forced to exit 5 on a host cause: drive 2 `skipped`, the cause named in the closing lines |
| Attended `will` banner | a record planted `will`: the dialog shows the banner, the session boots under the guard, the banner is in the job's lines again when the guard confirms | unattended on the same record: Upgrade disabled with `unattended.why`; the helper refuses a forced call |

Each proof's command and output lines go into the plan's task and the session record.

## 8. Carried issues

| bd | In stage 3 | How |
|---|---|---|
| `c8lf` (P2) | **fixed** | parallel pair mode starts each drive session with the default INT disposition (`bash -c 'trap - INT; exec bash "$SELF" session …' &`); suite case: parallel pair, SIGINT to the group, both children exit 3, no second step. Then the Cancel tooltip's caveat is removed. |
| `6obo` (P3) | **fixed** | the pair's firing is searched in the untruncated histories; the M10 edge takes the earliest line after the trigger. The GUI renders the result, so the fix belongs with it. |
| `k84b` (P3) | **closed by design** | decision 2: a 10-minute client timeout. |
| `epmw` (P3) | **read-only if cheap, else deferred** | if the status document gains an `orphans: [unit names]` list (helper, one key, fixture updated), the panel lists them with the manual-clear words from the disaster recovery guide; no clear-by-unit-name method in this stage. |
| `vrij`, `1ley`, `oz7w`, `f37n`, `gbgj` | tracked follow-ups | unchanged; named in the stage's closing report. |

## 9. Packaging and documentation

- **virt-viewer** optional: `optdepends` in `packaging/arch/PKGBUILD`, `Recommends` in
  `packaging/debian/control` and `packaging/fedora/das-backup-manager.spec`. Flatpak and
  Snap: a comment that the viewer is launched on the host, so inside a sandbox the path
  dialog is the way (no sandbox change in this stage).
- `docs/ARCHITECTURE.md`: the GUI section gains the panel and the model; the D-Bus client's
  method count.
- `docs/DISASTER-RECOVERY-GUIDE.md` §"In the recovery-os-updater VM": a subsection "From
  the GUI" (what each button does, the banner, needs-a-look, where the console path is).
- `CHANGELOG.md` `## [Unreleased]` → `### Added` (the panel, the console launch), `### Fixed`
  (`c8lf`, `6obo`), `### Changed` (packaging).

## 10. Out of scope

- Any new helper method; a `SessionEnd` job (decision 2 rejected it for now).
- Clearing orphan schedule units by name (`epmw`'s second half).
- Sandbox-side console viewing for Flatpak/Snap.
- Refactors `f37n` (panel.rs split) and `oz7w` (job-method insert under the lock).
