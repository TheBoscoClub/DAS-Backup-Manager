# Subvolume reconcile — back up what exists, not what was listed

- **Status:** design; expiry rule confirmed by operator 2026-10-01
- **Date:** 2026-10-01
- **Applies to:** `btrdasd` 0.7.22.x → next minor
- **Tracker:** bd `DAS-Backup-Manager-gte` (this work), bd `DAS-Backup-Manager-gya` (the incident)

## 1. The problem

The backup is an allowlist. A subvolume is backed up only if a
`[[source.subvolumes]]` entry names it. Creating a subvolume and protecting it
are separate acts joined only by memory, and `btrfs send` does not descend
into nested subvolumes, so a parent's snapshot holds an empty directory where
the child sits and the run reports success.

A detector exists (`btrdasd doctor --check-drift`, weekly, emails on drift).
It turns "remember to add a config entry" into "remember to act on an email".
The record of that failing:

| When | What |
|---|---|
| 2026-05-17 | Audit finds about 30 subvolumes never backed up |
| 2026-09-22, 09-23 | `bosco-media/video`, `@srv/stremio-web`, `@cache/stremio` created |
| 2026-09-27 | Weekly drift check finds all three and emails the report |
| 2026-10-01 | Still in no backup; added by hand (bd `gya`) |

Even the manual remedy has a second memory step: `btrdasd subvol add` writes
`config.toml` but does not regenerate `btrbk.conf`, so the subvolume is still
not backed up until `setup --upgrade` is also run.

## 2. The requirement

A subvolume that exists on a backed-up filesystem is backed up because it
exists. Nobody has to do anything that is not itself part of creating the
subvolume. Leaving a subvolume out is the act that takes a decision.

**Success test:** create a subvolume, do nothing else; the next backup run
contains it and its report says so.

## 3. Decisions taken (operator, 2026-10-01)

1. **Scoping of an adopted subvolume:** it inherits its parent's targets; a
   subvolume with no configured ancestor goes to the primary target only.
2. **What may keep a subvolume out:** only the explicit exclude list, where a
   pattern also covers everything nested under what it matches, plus
   snapshot trees. No guessing from names.
3. **A subvolume that disappears:** its entry is retired automatically and
   its existing backups expire under the target's retention.
4. **What "expire" means:** the whole series is kept until the retirement
   date plus the target's longest retention window, then deleted from that
   target together (section 9).

## 4. Approach

**Reconcile inside the run, and record the result.** Before btrbk starts, the
run compares what is on each source volume with `config.toml`, writes the
differences into `config.toml`, regenerates `btrbk.conf`, and carries on.

Rejected alternatives:

- *No per-subvolume config; generate `btrbk.conf` from disk each run.* Nothing
  would record what is protected, a retired subvolume would have nowhere to
  live, and snapshot names would have no stable home.
- *Hook subvolume creation.* There is no reliable hook; anything created
  another way bypasses it; it is a second mechanism that can stop silently.

## 5. Design

### 5.1 Units

| Unit | Kind | Responsibility |
|---|---|---|
| `adopt::plan_sync` (new module `indexer/src/adopt.rs`) | pure | config + per-volume subvolume listings → a plan: adopt / retire / revive / skip |
| `adopt::apply_plan` | pure over `Config` | returns the new `Config`; assigns sources, snapshot names, dates |
| `adopt::sync_subvolumes` | shell | lists subvolumes on mounted, verified volumes; saves config; regenerates `btrbk.conf`; returns a report |
| `expire::expire_retired` | shell over a pure selector | deletes retired series' snapshots that are past their window |
| `btrdasd subvol sync` | CLI | entry point for `backup-run.sh`; `--dry-run` prints the plan and changes nothing |
| `btrdasd subvol expire` | CLI | entry point for expiring retired subvolumes' backups after btrbk; `--dry-run` prints what would be deleted |

The words "reconcile" and `btrdasd reconcile` already mean something else in
this codebase (pruning index rows for snapshots that no longer exist), so the
new step is called **sync** in code and CLI. This document's title keeps the
design name.

The planner reuses what `doctor.rs` already has: the guarded lister
(`list_mounted_subvolumes`, which refuses a path that is not a real
mountpoint), `glob_match`, and `is_builtin_excluded`. Those move to the new
module or are shared; the logic is not duplicated.

### 5.2 Where it runs

In `scripts/backup-run.sh`, after `verify_sources_before_write` (every source
volume is mounted and proven to be the expected filesystem by UUID) and
before `create_snapshot_dirs`:

```
mount_sources → verify_sources_before_write → SYNC → reload config env
→ create_snapshot_dirs → mount_targets → … → run_btrbk → … → EXPIRE RETIRED
```

- It runs under the maintenance lock the run already holds.
- Under `--dryrun` it runs as `subvol sync --dry-run`: the plan is printed, nothing is written.
- Expiry is a second command, `subvol expire`, because it needs the targets mounted and sync runs before they are.
- The Rust manual path verifies each source volume's filesystem UUID and that
  it is mounted at its top level before listing it. The bash path already did
  this in `verify_sources_before_write`; the Rust path did not.
- The manual Rust path (`btrdasd backup run`) calls the same function before
  it invokes btrbk. There is no way to run a backup that skips sync.
- `btrdasd subvol add` / `remove` regenerate `btrbk.conf` too, so the manual
  route stops being a two-step trap.

Only `config.toml` and `btrbk.conf` are rewritten during a run. The installed
scripts are never touched, so the "never upgrade while a backup runs" hazard
(`.claude/rules/backup.md`) is unaffected. Both files are written to a
temporary file in the same directory and renamed into place.

### 5.3 Adopt

A subvolume found on a source volume that no entry names, and that is not
skipped (5.4), is adopted:

- **Nested** — its path has a configured ancestor on the same volume. It joins
  the nearest ancestor's source, and so takes that source's targets, snapshot
  directory and target subdirectories. It also takes the ancestor's
  `manual_only` flag.
- **Not nested** — it joins that volume's *adoption source*: a source named
  `<first-source-label-of-the-volume>-adopted`, created by sync the first
  time it is needed. It copies the volume, device and snapshot directory of
  the first source declared for that volume, sets `target_labels` to the
  primary target's label, and uses its own label as its target subdirectory.
  Targets are chosen per source in this codebase, which is why "primary only"
  is a source and not a flag.
- If no target has the primary role, a non-nested subvolume cannot be placed.
  It is not adopted, the run is marked failed (5.6), and the report names it.

Each adopted entry gets:

- `snapshot_name`, written explicitly at adoption: the existing algorithmic
  name, and if that collides with any snapshot name anywhere in the config,
  the same name with `-2`, `-3`, … appended. Existing names are never changed.
- `adopted = "YYYY-MM-DD"`.

`Config::validate()` gains a check that rejects duplicate snapshot names
within a source's snapshot directory, so a collision can no longer reach
btrbk. This closes bd `31q` for every path that goes through reconcile;
hand-written collisions are reported by validation.

### 5.4 Skip

A subvolume is left out for exactly two reasons:

1. **It is in a snapshot tree** — any path component is `.snapshots` or
   `.btrbk-snapshots` (the existing built-in rule).
2. **It matches the explicit exclude list**, or lies under a path that does.
   A pattern matches a path when it matches the path itself or any ancestor
   of it. `@cache` therefore covers `@cache/stremio`.

The exclude list moves to `[subvolumes] exclude`. `[doctor] exclude` is still
read, merged, and written back under the new key on the next save. The
built-in names `@tmp` and `@var-tmp` become default members of the list
instead of hard-coded special cases, so everything that keeps a subvolume out
is visible in one place.

`REBUILDABLE_PATTERNS` and `categorize()` are deleted.

Every skipped subvolume is listed in the run report with the reason and, for
an exclude match, the pattern. An exclusion is never invisible.
(Amended at the final review, Ruling 28: snapshot-tree skips are reported as
one count line per volume, not one line each; exclude-list skips stay
itemised with their pattern.)

### 5.5 Retire, expire, revive

**Retire.** An entry whose subvolume is absent from a volume that was listed
successfully gets `retired = "YYYY-MM-DD"`. Retired entries are not rendered
into `btrbk.conf`, so btrbk no longer exits 10 for them.

**Expire.** After btrbk, while targets are still mounted, each retired
series is checked on each target it was sent to, and in its source snapshot
directory:

- A retired series' snapshots are kept until the retirement date plus that
  target's **longest** retention window, then deleted together. A target
  with no retention configured has no window: its retired series are kept
  and reported, never deleted. With the
  live retention that is 7 days on each recovery drive and one year on the
  primary. Source-side snapshots follow the shortest window of the targets
  the series went to.
- This is deliberately simpler than re-implementing btrbk's daily → weekly →
  monthly thinning. Thinning exists to bound growth while new snapshots keep
  arriving; a retired series receives none, so the set is fixed and small.
  The rule errs toward keeping more.
- Deletion uses the existing `forget::delete_subvolume` and removes the
  matching index rows. It deletes only names of the form
  `<snapshot_name>.<timestamp>` for a retired entry, inside that entry's
  configured target subdirectory, on a path verified as a real mountpoint.
- When no snapshot of a retired series remains anywhere, the entry is removed
  from the config.

Windows are counted in whole days: a daily tier counts as 1 day, weekly as 7,
monthly as 31, yearly as 366. The rounding is deliberate and errs toward
keeping. Dates are UTC.

**Revive.** If a retired entry's subvolume is present again, `retired` is
cleared. btrbk decides whether the next send can be incremental.

The report carries a "Retired" section on every run while any retired entry
exists: the name, the retirement date, how many snapshots remain on each
target, and the date they expire. A subvolume deleted by mistake is announced
on every run for the whole window.

### 5.6 Failure behaviour

Every failure is loud and none is treated as "nothing to do".

| Condition | Effect |
|---|---|
| A source volume cannot be mounted, verified, or listed | Nothing on it is adopted, retired, or revived. The run continues on the existing config for that volume and is marked failed. |
| A listing succeeds but is empty | Treated as a failed listing. An empty answer is never read as "every subvolume was deleted". |
| `config.toml` or `btrbk.conf` cannot be written, or the new config fails validation | Neither file is replaced. The run continues on the previous config and is marked failed. |
| A non-nested subvolume cannot be placed (no primary target) | Not adopted; run marked failed; named in the report. |
| A retired snapshot cannot be deleted | Reported; retried next run. |
| A subvolume path contains a space | Read whole. The previous parser took the last word of the path; sync replaces it. |

"Marked failed" means the report says `FAILURES DETECTED` and
`backup_runs.success` is 0, as for any other failure. The exit-code contract
in `.claude/rules/backup.md` (Sentinel Interaction) is unchanged: the script
still exits 0 because the run executed.

There is no cap on how many subvolumes one run may adopt. A cap would be a
silent skip. The cost of adopting too much is space on the primary, the
report lists every adoption, and the exclude list is the remedy.

### 5.7 The weekly drift check

`btrdasd doctor --check-drift` stays, and its meaning changes. With reconcile
in place a missing subvolume means reconcile did not do its job. The check
becomes an independent alarm on the mechanism:

- **Missing** (on disk, not configured, not excluded): reconcile failed or
  has not run. Still exits 1, still emails.
- **Stale** (configured, not retired, not on disk): same.
- The "suggested config.toml additions" block is removed. The fix is no
  longer a hand edit.

### 5.8 Config schema

```toml
[subvolumes]
exclude = ["@tmp", "@var-tmp", "@cache", "coredumps",
           "@/var/lib/machines", "@/var/lib/portables"]

[[source.subvolumes]]
name = "bosco-media/video"
manual_only = false
snapshot_name = "bosco-media-video"
adopted = "2026-10-02"      # written by reconcile; absent on hand-made entries
# retired = "2026-11-14"    # written by reconcile when the subvolume is gone
```

`adopted` and `retired` are optional and default to absent, so every existing
config loads unchanged. An adopted entry is an ordinary entry: it can be
moved to another source, re-scoped, or marked `manual_only` by hand, and
reconcile leaves it alone afterwards.

## 6. Testing

- **Planner and selector: fixture tests**, no root, under the mutation gate.
  `adopt.rs` joins the weekly full scope in `.github/workflows/mutants.yml`.
  Cases include: nested and non-nested adoption; nearest-ancestor choice with
  two candidate ancestors; exclude covering nested paths; snapshot-tree paths;
  name collision; retire, expire-window boundaries on the day before, the
  day, and the day after; revive; a failed and an empty listing changing
  nothing.
- **Shell: seams as in `mount.rs`** (`CommandRunner`), so listing, saving and
  deleting are scripted in tests.
- **End to end, as root, on a VM** (`test-vm-cachyos`, loopback BTRFS source
  and target): create a subvolume, run `backup-run.sh`, find its snapshot on
  the target and its entry in the config. **Counter-test:** the same run with
  reconcile disabled must leave it absent. Then delete the subvolume, run
  again, observe retirement; advance past the window, observe expiry.
- **On this host:** `subvol sync --dry-run` must report nothing to adopt
  (the two `gya` subvolumes were added by hand on 2026-10-01) and
  `@cache/stremio` skipped by `@cache`. The first subvolume created after
  deployment is the live proof.

## 7. Documentation

- `.claude/rules/backup.md`, section "Nested Subvolumes Need Their Own Config
  Entry — Always", is rewritten: the entry is still needed, and the run
  creates it.
- `docs/INSTALL.md`, `docs/btrdasd.1`, `README.md`: the new subcommand, the
  `[subvolumes]` section, the report sections.
- `CHANGELOG.md`.

## 8. Out of scope

- Re-scoping or renaming subvolumes that are already configured.
- Changing how targets are chosen (still per source).
- bd `31q` for hand-written entries beyond the new validation error.
- bd `iyq` (all-zero retention becomes 4 weeks) as a whole. Expiry does need
  one rule from it: a target whose retention is all zero gives no window to
  measure against, so retired series on it are kept and reported on every
  run, never deleted.

## 9. Expiry rule — decided (operator, 2026-10-01)

Section 5.5's rule stands: a retired subvolume's snapshots are all kept until
the retirement date plus that target's longest retention window, then deleted
from that target together. With the live settings that is 7 days on each
recovery drive and one year on the primary.

btrbk-style thinning was considered and rejected. `btrbk prune` skips
deletion when the source is not accessible, so thinning a retired series
would mean a second implementation of btrbk's retention rules in this
codebase: selective, per-snapshot deletion, running unattended on what is by
then the only copy, and obliged to agree exactly with code we do not control.
The whole-series rule makes one decision per target and errs toward keeping
more. Its costs are accepted: more snapshots held for longer, and expiry as a
single date per target instead of a gradual fade.
