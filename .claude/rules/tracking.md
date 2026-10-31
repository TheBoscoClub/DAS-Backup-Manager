# Tracking — Where A New Issue Goes

bd owns the plan (global `beads.md`): one epic per phase, each epic `blocks` the next, every open issue is
a child of exactly one epic. This file is the procedure for keeping it that way; which issue sits in which
phase lives only in bd (`bd children <epic>`).

## Placement — a parent at filing, first rule that matches

1. **Blocks, or was discovered from, an issue already placed** → that issue's phase (the earliest, if several).
2. **P0, or P1 data loss / safety** → the phase in progress, at the top (Phase 1 while it is open). It does
   not wait behind the epic chain.
3. **By the files it touches** — a phase owns its files, so the work that must run serially stays together:
   restore → restore phase; `indexer/src/backup.rs`, `scripts/backup-run.sh` → backup-path phase; config
   writers, `setup`, GUI → config phase; real recovery drives → recovery-drive phase;
   `scripts/recovery-os-vm.sh`, the helper → recovery-os-vm phase; scrub → scrub phase; CI, dependencies,
   tooling → CI phase.
4. **Filed here by another project** → the same rules, at session-start triage.
5. **No rule fits** → ask the operator; never park it unparented.

```bash
bd create ... --parent <epic>          # at filing
bd update <id> --parent <epic>         # placing an existing issue
```

## Orphan check — session start and every `/checkpoint`

```bash
for s in open in_progress; do bd list --status=$s --no-parent --limit 0; done | grep -E ' P[0-4] ' | grep -v '\[epic\]'
```

Any line printed is an issue with no phase: place it before other work. Prints nothing when clean;
falsified 2026-10-30 (a detached issue printed, re-parented printed nothing).
