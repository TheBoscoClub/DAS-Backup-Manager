# ESP Safety (CRITICAL)

Installation specifics (which drives, serials, labels, mounts, bays): `.claude/rules/local-installation.md`
(gitignored, this machine only). Serials and UUIDs in tracked files are placeholders (`SERIALnn`, `5eed00nn-…`).

## This project never writes an ESP it does not own

Recovery drives attached to the DAS enclosure hold **fully independent OS installations** with their own
ESPs, labelled `RECOV-ESP-<bay>`. Device letters vary on reconnect: **identify by serial or PARTUUID, then
label** — a label is renameable and the weakest of the three. A shared label is itself a defect:
`scripts/das-wipe-all-drives-and-partition-from-scratch.sh` (v2.1.0+) derives the unique bay-numbered label and
refuses on collision (`tests/test_esp_label_derivation.sh`). Full account: `.claude/docs/rules-reference/esp-safety.md`.

### HARD RULE — beyond the global `~/.claude/rules/esp-ownership.md`

**The DAS ESPs are TOTALLY independent** of the host ESP — they boot their own OSes. That OS writing its
own ESP while booted (bare metal, the recovery-os-updater VM with its whole disk attached by serial, or the
live system inside that VM during `provision`) is permitted; the host still never writes `RECOV-ESP-*` and
never mounts a drive the VM holds.

### Incident behind these rules

The hook in the global rule's 2026-03-05 incident came from this project's own `render_esp_hook()`
(`indexer/src/setup/templates.rs`); it, `EspHooks`/`HookType` and the wizard's hook prompt were deleted. **No code path here can generate an ESP sync hook — keep it that way.**
A host-ESP mirror is a different system — never conflate the two: it must hardcode its paths, have no
discovery loop, cross-check label against mount, and refuse any device not on the expected bus class.

### The one ESP mount this project makes

The recovery-OS reading mounts a mirror target's own ESP **read-only** (bd `sp78.20`), only by that target's
`esp_partuuid`, only when the drive itself lists that PARTUUID, never over or beside another mount of it:
`mount::read_recovery_esp`. No other code path mounts an ESP; never add one that finds it any other way.
One left mounted is released only at its own directory, only when `findmnt -S PARTUUID=<esp_partuuid>` shows
it there: `MountGuard::release_esps` (bd `s5jv.2`), `backup-run.sh` `unmount_all` (bd `s5jv.1`).

### Target mounts

The targets are mounted by `backup-run.sh` / `btrdasd` only, hidden from udisks2 by a generated ignore rule
(`sudo btrdasd setup --check`).
