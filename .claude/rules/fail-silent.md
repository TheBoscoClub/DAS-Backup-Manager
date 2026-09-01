# Fail-Silent Suppressions — What Is Legitimate In This Codebase

The defect class itself is defined globally in `~/.claude/rules/verification.md`
§2 ("Hunt the silent failure before declaring success"). **This file does not
restate it.** It records the project-specific judgment: which suppression shapes
in *this* tree are deliberate and correct, which are defects, and how to tell
them apart — so the next audit is a **diff against this list**, not a re-read of
the whole tree.

Tracks `bd DAS-Backup-Manager-nsp`.

---

## The test: which direction does the substituted default point?

Every suppression replaces an error with an assumption. The shape of the code
(`.ok()`, `unwrap_or`, `2>/dev/null`, `|| true`) tells you **nothing** about
whether it is a defect. The only question that matters is:

> **When this swallows an error, does the value it substitutes make the caller
> more cautious, or less?**

- **Substitutes the cautious answer → legitimate.** The failure degrades into a
  refusal, a retry, or a warning. Nothing proceeds on a fiction.
- **Substitutes the permissive answer → defect.** The failure degrades into
  "fine", and the caller acts on an assurance nobody checked.

`health::is_mountpoint` is the canonical legitimate case here: it returns
`false` when `/proc/mounts` cannot be read. That is an error being discarded —
and it is correct, because `false` sends `mount::verify_write_targets` into a
refusal. **The identical `Err(_) => false` would be a defect at a call site
where `false` meant "permitted".** Same shape, opposite verdict. Judge the call
site, never the pattern.

---

## Legitimate in this tree (do not re-file these)

1. **Best-effort unmount on an error path already being reported.**
   `guard.unmount(&progress)` in the D-Bus helper's job closures runs while an
   error is on its way to `emit_job_finished`. An unmount failure there cannot
   change the outcome already being reported, and `MountGuard::drop` is the
   backstop. Logged, not propagated.

2. **Indexing errors do not abort a backup.** Documented soft-fail: the backup
   is the product, the index is a convenience over it. An index that failed to
   update is recoverable by re-running the indexer; a backup that did not run
   is not. The failure must still reach the report — silence is not part of the
   contract, only non-abortion is.

3. **Probing for optional tooling.** `findmnt`, `smartctl`, `btrfs` absent or
   erroring resolves to `None`/`unknown`, never to a fabricated measurement.
   See the hard rule on sentinel values below.

4. **Mount attempts for individual targets.** `ensure_targets_mounted` logs a
   per-target failure and continues, because one absent drive must not stop a
   backup to the others. **This is legitimate only because the target is then
   excluded** — the failure branches must not mark it available. Two of them
   did, and that was `bd DAS-Backup-Manager-aea`.

5. **`backup-run.sh` exits 0 on per-target failure.** Deliberate, and NOT to be
   "fixed" without reading the reasoning: nonzero means *the run could not
   execute at all*. A multi-hour job never produces three failures inside
   cachyos-sentinel's 600-second restart-limiter window, so a per-target
   nonzero would produce an unbounded retry loop rather than an alert. Recorded
   in full under `bd DAS-Backup-Manager-18p` and in `backup.md`.

### Bash inventory, triaged 2026-09-01 (bd `76g`)

All 54 hits of the bash grep below were classified. Everything fell into these
shapes, each legitimate because the substituted value is the **cautious** one —
so a future audit is a diff against this list, not a re-read:

| Shape | Substitutes | Why cautious |
|:--|:--|:--|
| `smartctl`/`blkid`/`findmnt`/`btrfs` probe → `unknown`, `?`, empty | "no reading" | never a fabricated measurement; `usb_link_mbit_s` stays a **string** for this reason |
| `mountpoint -q … 2>/dev/null` | "not a mountpoint" | sends callers into skip/abort, which is the safe branch |
| `btrfs filesystem label … \|\| echo "$mnt"` | the mount path | display-only fallback |
| report gathering (`df`, `btrbk list latest`) → blank | "no data" | costs a report row, never a decision |
| `date -d … \|\| echo 0` | epoch 0 | **explicitly handled**: `boot-archive-cleanup.sh` logs and `continue`s rather than deleting; the growth-log reader skips the entry |
| `echo … > /sys/…/scheduler \|\| true` | kernel default | a performance hint, not correctness |

Two of these were checked closely because a `0` sentinel on a **deletion** path
would be the permissive direction. Both are safe: `parse_archive_timestamp`
returning `0` reaches `if (( archive_epoch == 0 )); then log_warn; continue`, so
an unparseable archive is **skipped, never pruned**.

One real defect was found and fixed: `resolve_fs_label` in
`das-partition-drives.sh` read `blkid`'s empty output as "unlabelled", which
conflates *no LABEL* with *blkid could not tell me*. On the second reading the
caller went on to **write** the fallback label — silently renaming a filesystem,
the exact renaming that function exists to prevent (`bd 5j7`). Now split by exit
status: `0` found, `2` genuinely unlabelled, anything else fails closed. Two
regression cases in `tests/test_esp_label_derivation.sh`, both observed RED
against the pre-fix code with `guard did NOT fire (exit 0)`.

### Rust inventory, triaged 2026-09-01 (bd `8wx`)

All **236** hits of the Rust grep below were classified — 223 in production
code, 13 inside `#[cfg(test)]` modules. **200 (a) / 16 (b) / 20 (c)**, counting
grep hits rather than defect sites (one defect can span seventeen lines).

The (a) shapes, each legitimate because the substituted value is the **cautious**
one. A future audit is a diff against this table:

| Shape | Substitutes | Why cautious |
|:--|:--|:--|
| `Command::new(..).output().ok()?` on `blkid` / `findmnt` / `smartctl` / `btrfs` / `which` / `systemctl` | `None`, `"unknown"`, `"N/A"` | probe for optional tooling; never a fabricated measurement |
| `parse().ok()?` inside a fn returning `Option` (timestamps, subvol ids, `hh:mm`, lsblk rows) | `None` → the row/answer is skipped | an unparseable input is dropped, never guessed |
| `.map(..).unwrap_or(false)` on `mountpoint -q`, `systemctl is-enabled`, `read_dir` | "not mounted", "not enabled" | sends callers into mount/refuse/warn — the safe branch |
| `Err(_) => return false` in `health::is_mountpoint` | "not a mountpoint" | the canonical case; drives `verify_write_targets` into refusal |
| `.unwrap_or("<none>" / "-" / "N/A" / "unknown")` in report and CLI table rendering | a placeholder cell | display only; no decision reads it |
| `.await.unwrap_or_else(\|e\| Err(format!("… panicked: {e}")))` on every helper job | a reported failure | converts a panic into an honest `JobFinished(success=false)` |
| `let _ = child.kill()` / `umount` / `remove_file` **on a path already returning Err** | nothing | the outcome is already being reported; `MountGuard::drop` is the backstop |
| `let _ = HelperInterface::job_*(..)` D-Bus signal emission | nothing | fire-and-forget telemetry from a detached task, nothing downstream reads it |
| `canonicalize().unwrap_or_else(\|_\| literal)` in `check_source_allowed` | the unresolved root | strictly **narrows** the allow-list: a canonical path can never start with a symlinked prefix, so the fallback can only refuse more |
| `env::var(..).unwrap_or_else(\|_\| default)` (`EDITOR`, `DAS_SCRUB_STATE`, `DAS_REPORT_TO`) | the documented default | configuration, not error suppression |
| `Option::unwrap_or` on a genuinely-absent map/vec entry (`serials.first()`, `target_subdirs.first()`) | a documented default | no error exists to swallow |
| `let _ = conn.execute_batch("PRAGMA optimize")` in `Drop` | nothing | `Drop` cannot propagate, and the data is already committed |
| bare `remove_dir(dir)` as a "delete it if it is now empty" sweep | nothing | failing **is** the normal outcome when the operator left files there |
| `SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(0)` | epoch 0 | only reachable with a pre-1970 clock; 0 reads as "ancient", the cautious side of every freshness check — see the caveat below |

Four **(c)** correctness bugs were found and fixed, each with a counter-test
observed RED against the pre-fix code:

- `scrub.rs` — a **recognised** counter with a non-numeric value fell back to
  `0`, and `0` on `uncorrectable_errors` is "no damage": a truncated or
  corrupted `scrub.status.<uuid>` record parsed as `finished: true`, all
  counters zero, `is_clean()` **true**. No FAILURE email, health green. Unknown
  *keys* are still ignored — that is the forward-compat contract and it is
  separately tested.
- `health.rs` — a **mounted** target whose capacity could not be read reported
  `(0, 0)`, and `determine_status` skips its `>95 %` escalation when
  `total_bytes == 0`. The one target nobody could measure was the one target
  that could never raise the disk-full alarm, drawn as 0 % used. Measurement is
  now `measure_target_usage() -> Option`, and `None` on a mounted target pushes
  a warning.
- `setup/mod.rs` — `setup --modify` used `Config::load(..).ok()`, collapsing
  "no config yet" and "config I cannot parse" into `None`. The second sent the
  wizard to its defaults and `install()` then wrote those defaults over the file
  the operator asked to *modify*. Now `Ok(None)` means absent, and unreadable is
  an error.
- `installer.rs` — `uninstall_from_manifest` dropped every `remove_file` error
  via `.is_ok()` and returned a bare `0` for an unreadable manifest, so an
  uninstall that removed nothing printed "Removed 0 files." and "Uninstall
  complete." It now returns `(count, problems)`.

The **(b)** fixes: the helper's background `IndexStats` refresh (its whole
result was `let _ = ..`, so a DB it could no longer open served a permanently
stale dashboard in silence), the install-time DB-directory creation, the
uninstall paths that leave timers enabled or files behind, and
`verify_write_targets` reading "could not list this path" as "empty, nothing to
see" — a plain FILE at a mount point failed `read_dir` with `ENOTDIR` and
produced no log line at all.

Two things deliberately left, named so they are not re-litigated:

- **`serde_json::to_string(..).unwrap_or_else(\|_\| "[]")`** in four helper
  D-Bus methods. Wrong direction — "[]" tells the GUI *there is nothing there* —
  but `serde_json::Value` cannot hold an unserializable value (no NaN, no
  non-string keys), so it is unreachable and **no counter-test can be
  constructed**. Left rather than changed untested. If any of these ever
  serialises a raw `f64`, it becomes a live defect.
- **`scanner.rs`'s `Err(_) => (0, 0)`** for a file whose metadata will not read.
  The `0` size and mtime land in the index and the span logic reads them as a
  real measurement — but the entry is counted in `errors` and surfaced, and the
  row must carry two `i64`s. Skipping the entry instead is a behaviour change to
  the indexer that wants its own verification cycle. **Flagged, not fixed.**

Caveat on the `unwrap_or(0)` clock reads (`main.rs:759`, `health.rs:657`,
`btrdasd-helper.rs:1532`): the helper's is the one that inverts, because
`now_secs - backup_secs` with `now_secs == 0` yields a *negative* age, which
renders as "just backed up". Unreachable without a pre-1970 clock, so no test
was written.

---

## Always a defect here

1. **A sentinel that is indistinguishable from a measurement.** A missing
   reading is `"unknown"` or `None` — never `0`. `throughput.jsonl` records
   `usb_link_mbit_s` as a **string** for exactly this reason. A numeric `0`
   cannot be told apart from a real zero, and `backup-verify.sh` printed
   fabricated zero sector counts *in green* for months on that mistake.

2. **A failure branch that records success.** Any assignment of an
   availability, health, or success flag inside an `Err`/failure arm. This is
   the `aea` shape and it is never correct.

3. **A truncated verification.** `| head`, `-c N`, or a `grep` for the success
   string only, in anything whose purpose is to check. If the command cannot
   print bad news, it is not a check. Two live instances of this were found in
   this project's own tooling.

4. **A parameter, flag, or setting that is accepted and ignored.** The GUI's
   `--db` option and its `DatabasePath` settings field were both removed rather
   than left inert when the helper stopped trusting caller-supplied paths
   (`bd DAS-Backup-Manager-gko`). An option that silently does nothing is a
   fail-silent defect wearing a feature's clothes.

5. **`local x=$(cmd)` masking `cmd`'s exit status**, and any pipeline whose
   producer is killed by SIGPIPE under `set -o pipefail`. The
   `backup-verify.sh` serial parse died this way, so every drive reported
   `Unknown` and an entire disk-usage section had **never executed** on this
   host.

---

## Auditing this as a diff

Re-deriving the whole inventory costs a full pass over `indexer/src` and
`scripts/`. Do not do that unless the tree has changed structurally. Instead:

```bash
# Rust: candidate suppressions
grep -rn '\.ok()\|unwrap_or\|let _ =\|Err(_) =>' indexer/src --include=*.rs

# Bash: candidate suppressions
grep -rn '2>/dev/null\|)| true\|)| :' scripts/*.sh
```

For each hit **not already covered by the legitimate list above**, apply the
direction test in one line: *what does this substitute, and does it make the
caller more cautious or less?* Only the "less" answers need triage.

Classify into three buckets and treat them differently:

| Bucket | Meaning | Disposition |
|:---------------------|:-------------------------------|:--------------------------|
| (a) legitimate | default is the cautious one | leave; extend this file |
| (b) should-log | non-fatal, silence costs diagnosis | log at warn, continue |
| (c) should-propagate | changes the caller's decision | return an error |

**(c) is the only bucket that is a correctness bug.** (b) is a diagnosability
debt — real, worth fixing, but it does not make the system wrong. Do not let a
large (b) count delay a (c) fix.

---

## Do not trust an audit's confidence, including your own

Both agents that produced the original inventory over-claimed exactly once, in
opposite directions, and both were caught only by checking against reality:

- One asserted the boot-subvolume snapshot finder had matched nothing "for the
  entire v4.x history". The journal shows `Latest root: nvme/root-.20260828T0300`.
  It matches. The real defect was narrower — a second implementation of the
  naming rules with no drift protection.
- The other correctly identified the `backup-verify.sh` serial defect, and the
  first attempt to reproduce it **failed** because the reproduction omitted
  `set -o pipefail`. Under the script's real options it reproduces exactly. The
  agent was right; the check was wrong.

The lesson is symmetric: **reproduce under the real conditions before believing
either a finding or a refutation.** A reproduction that omits the production
shell options, environment, or privileges is not evidence in either direction.

---

## Related

- `~/.claude/rules/verification.md` — the class, and the both-directions test rule
- `backup.md` — the `18p` exit-code split, in full
- `bd DAS-Backup-Manager-nsp` — the audit this file closes out
