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
