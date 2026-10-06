# Fail-Silent Suppressions — What Is Legitimate In This Codebase

The defect class is defined globally in `~/.claude/rules/verification.md` §2 and is not
restated here. This file records the project-specific judgment, so the next audit is a
**diff against a known list**, not a re-read of the tree.

The full triaged inventories (54 bash hits, 236 Rust hits, shape by shape), the defects they
found, and the two deliberately-unfixed cases are in `.claude/docs/rules-reference/fail-silent.md`,
which is NOT auto-loaded. **Read it before auditing suppressions or filing one as a defect** —
most candidates are already classified there. Audit: `bd DAS-Backup-Manager-nsp` (closed).

## The test: which direction does the substituted default point?

Every suppression replaces an error with an assumption. The shape of the code (`.ok()`,
`unwrap_or`, `2>/dev/null`, `|| true`) tells you **nothing**. The only question:

> **When this swallows an error, does the value it substitutes make the caller more
> cautious, or less?**

- **Cautious → legitimate.** The failure degrades into a refusal, a retry, or a warning.
- **Permissive → defect.** The failure degrades into "fine", and the caller acts on it.

`health::is_mountpoint` returning `false` when `/proc/mounts` is unreadable is the canonical
legitimate case: `false` drives `mount::verify_write_targets` into refusal. The identical
`Err(_) => false` is a defect wherever `false` means "permitted". **Judge the call site,
never the pattern.**

## Legitimate in this tree (do not re-file these)

1. **`MountGuard::drop`'s unmount** — backstop, logged only; an explicit `unmount()` that
   leaves a mount fails its job (`5oc`).
2. **Indexing errors do not abort a backup** — but the failure must still reach the report.
3. **Probing for optional tooling** (`findmnt`, `smartctl`, `btrfs`, `blkid`) resolves to
   `None`/`unknown`, never to a fabricated measurement.
4. **Per-target mount failure is logged and the run continues** — legitimate *only because the
   target is then excluded*. A failure branch must never mark it available (`bd aea`).
5. **`backup-run.sh` exits 3 — which its units count as success — when it began its work and
   something failed** — deliberate, the doctor's rule; see `backup.md` §Sentinel Interaction
   (`bd d1r`). The failure travels by the report, the history row and the journal's `status=3`,
   never by a restart; only a run that could not start exits 1 and fails the unit. An abort
   before the report stage sends one ABORTED report and records a failed row (`bd 2my`); one
   after the report went out has only the journal's `status=3` and the log as its trace.
6. Display-only fallbacks, documented config defaults, fire-and-forget D-Bus signals, and
   errors discarded in `Drop` or on a path already returning `Err`.

## Always a defect here

1. **A sentinel indistinguishable from a measurement.** A missing reading is `"unknown"` or
   `None` — never `0`. `usb_link_mbit_s` is a **string** in `throughput.jsonl` for this reason.
2. **A failure branch that records success** — any availability, health or success flag
   assigned inside an `Err`/failure arm.
3. **A truncated verification** — `| head`, `-c N`, or a grep for the success string only, in
   anything whose purpose is to check.
4. **A parameter, flag or setting that is accepted and ignored.** Remove it; do not leave it inert.
5. **`local x=$(cmd)` masking `cmd`'s exit status**, and any pipeline whose producer dies of
   SIGPIPE under `set -o pipefail`.
6. **Empty output read as a negative answer** — "blkid printed nothing" is not "unlabelled".
   Split on exit status and fail closed: a `flock` that failed is not one that found the lock
   held (`bd ismb`).

## Auditing this as a diff

```bash
grep -rn '\.ok()\|unwrap_or\|let _ =\|Err(_) =>' indexer/src --include=*.rs
grep -rn '2>/dev/null\|)| true\|)| :' scripts/*.sh
```

For each hit not already covered by the reference inventory, apply the direction test.

| Bucket | Meaning | Disposition |
|:--|:--|:--|
| (a) legitimate | default is the cautious one | leave; extend the reference |
| (b) should-log | non-fatal, silence costs diagnosis | log at warn, continue |
| (c) should-propagate | changes the caller's decision | return an error |

**(c) is the only correctness bug.** Do not let a large (b) count delay a (c) fix.

## Do not trust an audit's confidence, including your own

**Reproduce under the real conditions before believing either a finding or a refutation.** A
reproduction that omits the production shell options (`set -o pipefail`), environment or
privileges is not evidence in either direction. Both original audit agents over-claimed once,
in opposite directions; each was caught only by checking against the running system.

## Related

- `~/.claude/rules/verification.md` — the class, and the both-directions test rule
- `backup.md` — the exit-code splits: `18p` (scrub), `d1r` (backup)
