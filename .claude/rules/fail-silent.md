# Fail-Silent Suppressions — What Is Legitimate In This Codebase

Defect class: `~/.claude/rules/verification.md` §2 (and its both-directions test rule). This file
records project-specific judgment, so the next audit is a **diff against a known list**.
Full triaged inventories (54 bash hits, 236 Rust hits), the defects found and the two deliberately
unfixed cases: `.claude/docs/rules-reference/fail-silent.md` (NOT auto-loaded). **Read it before
auditing suppressions or filing one as a defect.** Audit: `bd DAS-Backup-Manager-nsp` (closed).

## The test: which direction does the substituted default point?

Code shape (`.ok()`, `unwrap_or`, `2>/dev/null`, `|| true`) tells you **nothing**. Ask: **does the
substituted value make the caller more cautious, or less?**
- **Cautious → legitimate** (degrades into refusal, retry, or warning).
- **Permissive → defect** (degrades into "fine", and the caller acts on it).

Canonical legitimate case: `health::is_mountpoint` returning `false` when `/proc/mounts` is unreadable
drives `mount::verify_write_targets` into refusal. The same `Err(_) => false` is a defect wherever
`false` means "permitted". **Judge the call site, never the pattern.**

## Legitimate in this tree (do not re-file these)

1. **`MountGuard::drop`'s unmount** — backstop, logged only; an explicit `unmount()` that leaves a mount fails its job (`5oc`).
2. **Indexing errors do not abort a backup** — but must still reach the report.
3. **Probing optional tooling** (`findmnt`, `smartctl`, `btrfs`, `blkid`) resolves to `None`/`unknown`, never a fabricated measurement.
4. **Per-target mount failure is logged and the run continues** — legitimate *only because the target is then excluded*. A failure branch must never mark it available (`bd aea`).
5. **`backup-run.sh` exits 3 — counted as success by its units — when it began its work and something failed** — deliberate, the doctor's rule; see `backup.md` §Sentinel Interaction (`bd d1r`). Failure travels by the report, the history row and the journal's `status=3`, never by a restart; only a run that could not start exits 1 and fails the unit. An abort before the report stage sends one ABORTED report and records a failed row (`bd 2my`); one after the report went out leaves only the journal's `status=3` and the log.
6. Display-only fallbacks, documented config defaults, fire-and-forget D-Bus signals, and errors discarded in `Drop` or on a path already returning `Err`.

## Always a defect here

1. **A sentinel indistinguishable from a measurement.** Missing reading is `"unknown"` or `None`, never `0`; hence `usb_link_mbit_s` is a **string** in `throughput.jsonl`.
2. **A failure branch that records success** — any availability, health or success flag set inside an `Err`/failure arm.
3. **A truncated verification** — `| head`, `-c N`, or grep for the success string only, in anything whose purpose is to check.
4. **A parameter, flag or setting accepted and ignored.** Remove it; don't leave it inert.
5. **`local x=$(cmd)` masking `cmd`'s exit status**, and any pipeline whose producer dies of SIGPIPE under `set -o pipefail`.
6. **Empty output read as a negative answer** — "blkid printed nothing" is not "unlabelled". Split on exit status and fail closed: a `flock` that failed is not one that found the lock held (`bd ismb`).

## Auditing this as a diff

```bash
grep -rn '\.ok()\|unwrap_or\|let _ =\|Err(_) =>' indexer/src --include=*.rs
grep -rn '2>/dev/null\|)| true\|)| :' scripts/*.sh
```

Apply the direction test to each hit not already in the reference inventory.

| Bucket | Meaning | Disposition |
|:--|:--|:--|
| (a) legitimate | default is the cautious one | leave; extend the reference |
| (b) should-log | non-fatal, silence costs diagnosis | log at warn, continue |
| (c) should-propagate | changes the caller's decision | return an error |

**(c) is the only correctness bug.** A large (b) count must not delay a (c) fix.

## Do not trust an audit's confidence, including your own

**Reproduce under real conditions before believing a finding or a refutation.** A repro omitting
production shell options (`set -o pipefail`), environment or privileges is not evidence either way.
Both original audit agents over-claimed once, in opposite directions; each was caught only against the running system.

## Related

- `backup.md` — the exit-code splits: `18p` (scrub), `d1r` (backup)
