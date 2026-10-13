# Recovery Drive Provisioning Implementation Plan (plan 2 of 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A recovery drive's OS can be rebuilt from the host by one command, `recovery-os-vm.sh provision <A|B|label> --iso <path> --package <path> --package-sha256 <hex> --authorized-keys <file>`, without the host writing a byte to the drive, without reformatting the partition that holds backup data, and with every new piece (marker, sync unit, timers masked, previous-root entry, splash messages) in place on the first boot.

**Architecture:** (1) A guest-side installer, `scripts/recovery-os-install.sh`, runs inside the CachyOS live ISO in the drive's own VM: it replaces the OS subvolumes on p2 by an explicit allowlist (renamed, never deleted until the new OS booted), formats p1, pacstraps the baseline, configures, installs the project package, writes the loader entries, and verifies; `undo` renames the old subvolumes back. (2) The driver gains `provision`: preflight on the host (locks, the other drive's state, pinned sha256s, a capture of the old OS), a payload ISO built with xorriso carrying the installer and the package, the live ISO as a boot-order-1 cdrom, each installer step run through the existing `run_stage` machinery, a Phase 3 first boot under the session guard with the proof the spec names, deletion of the `*.replaced.<stamp>` subvolumes only after that proof, a history row, `--dry-run` and `--undo`. (3) The sync unit (plan 1) gains D9: a header and one line per phase on the splash through `plymouth display-message`, each phase's duration measured into a timings file outside the state directory, and a countdown only from the previous boot's measurement. (4) D1: a `loader` step in the unattended pipeline rewrites `cachyos-recovery-previous.conf` to the newest `@.pre-update.*` after each upgrade; the installer writes no such entry (no snapshot exists yet).

**Tech Stack:** bash (`scripts/recovery-os-vm.sh` 2.1.2 → 2.2.0, new `scripts/recovery-os-install.sh`, `scripts/das-recovery-state-sync.sh` 1.0.2 → 1.1.0), the project's bash test style (`tests/test_recovery_os_vm.sh`: the whole driver run against stub tools under a `mktemp` root; `tests/test_recovery_state_sync.sh`: seams per tool, `check <name> <command>`), xorriso (`libisoburn`, installed on the host), libvirt (a second SATA cdrom), systemd-boot, plymouth, CMake install lines.

**Spec:** `docs/superpowers/specs/2026-10-12-recovery-drive-provisioning-design.md` §3 (what a rebuilt drive looks like), §4 Phases 0–3 (plan 3 owns Phases 4–5), §5 (the component table), decisions D1, D2, D3, D5, D6, D7, D9. Plan 1 (`2026-10-12-recovery-state-plan.md`) built D4 and D8 and the sync unit this plan extends.

## Global Constraints

- **The host writes nothing to a recovery drive** — not p1, not p2, not even a format (spec §2.1). Every write in this plan is a guest-exec inside the VM. The driver's only writes are to its own state under `/var/lib/das-backup/`, the payload ISO under `/var/lib/libvirt/images/`, and the domain definition.
- **No partition table is written and no btrfs filesystem is made anywhere in the flow** (spec §3.1). p2 is identified by the target's `mount_uuid`; a mismatch is a refusal before any rename. `das-wipe-all-drives-and-partition-from-scratch.sh` is not called and gains no one-drive mode.
- **The OS allowlist is explicit and exact**: names `@`, `@home`, `@rescue`; prefixes `@.pre-update.`, `@.half-upgraded.`; plus the names given one by one with `--also-replace` (each must start with `@.archive.`; D7 is drive A's two archives, named in plan 3). Anything else on p2's top level is kept untouched. No glob ever reaches `btrfs subvolume delete`.
- **Deletion only after proof**: `*.replaced.<stamp>` subvolumes are deleted inside the VM by the new OS, each by name, only after Phase 3's first boot proved the desktop. A stopped provision leaves them; `--undo` renames them back and refuses if a new `@` has booted since.
- **The recovery OS never runs btrbk, the backup timers or a scrub** (spec §2.2): the installer masks `das-backup.timer`, `das-backup-full.timer`, `das-scrub.timer`, `das-backup-doctor.timer`, `btrbk.timer`, `btrbk.service`, and the `verify` step must read `btrbk at boot: no` from `btrdasd recovery-os inspect` against the new root.
- **Repository order**: the four `[cachyos*]` sections before `[core]`/`[extra]`; the existing `step_repos` script is run against the new root's `pacman.conf` (`PACMAN_CONF=/mnt/target/etc/pacman.conf`) — the same check the pipeline runs, not a second one.
- **Pinned inputs**: `--iso` must match `DAS_RECOVERY_VM_ISO_SHA256` (default: the sha256 of `cachyos-desktop-linux-260809.iso`, read from `/hddRaid1/ISOs/cachyos-desktop-linux-260809.iso.sha256` when the executor runs Task 3a and written into the script as the default — never left empty); `--package` must match `--package-sha256` (required; plan 3 passes the release asset's value). The package pinned is **0.7.23.0** (D3), promoted before plan 3.
- **Identity by serial and PARTUUID**, never `/dev/sdX`; labels stay `RECOV-ESP-<bay>` and `das-backup-system-recovery-<A|B>`. The bay comes from `display_name` by the same regex `derive_esp_label` uses in `das-wipe-all-drives-and-partition-from-scratch.sh:457` (`[Bb]ay[[:space:]]*([0-9]+)`); no bay number is a refusal.
- **Package sources**: CachyOS repos → Arch repos → Flathub remote added with no apps (D6). Nothing from AUR; no AUR helper installed.
- **Fail-silent rule** (`.claude/rules/fail-silent.md`): every step's failure direction is observed in its test; no step records success in a failure branch; the installer's `verify` fails on any missing file the loader entries name.
- **Exit codes of `provision`** follow the driver's table: 0 done; 1 refused, nothing held; 2 usage; 3/4 kept; 5 done with warnings; 7 stopped at a step (named), the live system powered off, the disk given back. `provision` never leaves a unit `failed`: it runs under the same `SuccessExitStatus=1 3 4 5 6 7` rule as `session` when detached.
- bash `#!/bin/bash`, `set -euo pipefail` in the driver, `shellcheck -S warning` clean; Rust untouched by this plan; `cargo` never run bare; build in `/tmp/das-backup-build`; no `cmake --install` while a backup runs (busy check in `.claude/rules/backup.md`).
- No AI attribution in any commit. `.staged-release` (0.7.23.0) stays: commit pushes only, no tag.
- Script headers carry `# Version:`; bump them when edited (driver 2.2.0, sync 1.1.0, installer 1.0.0).

## Review Focus

1. **A received backup subvolume whose name looks like an OS one** (`@home-old`, `@.pre-update-notes`, `@rescue2`, a stray `root-.20261012T0348` at the top level): the allowlist must match whole names and exact prefixes, never substrings; the suite creates received subvolumes with each of these names and watches `replace-os` leave them alone. Pinned in Task 1's "allowlist is exact" test.
2. **p2 is the right UUID but holds no received backups** (a drive wiped by the fresh-enclosure tool and never re-seeded): `replace-os` refuses — an empty disk is not a recovery drive to rebuild, it is one to seed first. Pinned in Task 1's "empty disk" test.
3. **A provision stopped after `replace-os` and before Phase 3**, then the operator runs `session` or the nightly backup: the drive must still be refused by the pipeline (no boot record for a new OS) and the backup must still receive into the untouched subvolumes; `--undo` must restore the old OS by name. Pinned in Task 1's "undo" tests and Task 3c's "undo refused by the installer" test.
4. **The first boot after a rebuild**, where the sync unit finds no `das-state` snapshot (exit 2) and no timings file: the splash shows the header and the phase text only, no countdown, and the unit leaves no database. Pinned in Task 2's "first boot, no countdown" test and Task 3b's proof.
5. **A power-off during the `pacstrap` step** (the VM's shutdown at a limit): p2 holds a half-filled `@` and the old OS under `*.replaced.<stamp>`; a re-run of `provision` must start again cleanly: `replace-os` finds the partial `@` on the allowlist (it is named `@`) and sets it aside as `@.replaced.<new stamp>` — never deleting it — while the first run's `*.replaced.<old stamp>` stay. Pinned in Task 1's "re-run after a partial install" test.

---

### Task 1: `scripts/recovery-os-install.sh` — the guest-side installer

**Files:**

- Create: `scripts/recovery-os-install.sh`
- Create: `tests/test_recovery_os_install.sh`
- Modify: `CMakeLists.txt:139-146` (install line), `CMakeLists.txt` `add_test` block (~line 329)

**Interfaces:**

- Consumes: nothing from the driver at run time except its environment (below) and the payload paths.
- Produces: one script, `recovery-os-install.sh <step>`, steps `replace-os`, `pacstrap`, `configure`, `package`, `boot`, `verify`, `unmount`, `delete-replaced`, `undo`, plus `--count-packages` (prints the baseline's package count, for the driver's dry run). Parameters are environment variables, every one prefixed `DAS_INSTALL_`, set by the driver's step scripts (Task 3b):

| Variable | Meaning |
| --- | --- |
| `DAS_INSTALL_UUID` | p2's filesystem UUID (the target's `mount_uuid`) — refused if the disk's p2 differs |
| `DAS_INSTALL_ESP_LABEL` | `RECOV-ESP-<bay>` |
| `DAS_INSTALL_BAY` | the bay number |
| `DAS_INSTALL_LETTER` | `A` or `B` |
| `DAS_INSTALL_HOSTNAME` | `das-recovery-bay<N>` |
| `DAS_INSTALL_STAMP` | `YYYYMMDD-HHMMSS`, one per provision run, used in every `*.replaced.<stamp>` name |
| `DAS_INSTALL_ALSO_REPLACE` | space-separated extra names, each must start with `@.archive.` |
| `DAS_INSTALL_PACKAGE` | path of the project package inside the live system (on the payload cdrom) |
| `DAS_INSTALL_KEYS` | path of the `authorized_keys` file on the payload cdrom |
| `DAS_INSTALL_PAYLOAD_SHA256` | the package's expected sha256 (checked again inside the guest) |
| `DAS_INSTALL_DEV` | the drive's whole-disk node inside the VM (`/dev/sda`) |
| `DAS_INSTALL_ROOT` | **test seam**: a fixture tree standing in for `/`; empty in production |
| `DAS_INSTALL_TOP`, `DAS_INSTALL_TARGET` | mount points, default `/mnt/top`, `/mnt/target` |
| `DAS_INSTALL_BTRFS`, `_MOUNT`, `_UMOUNT`, `_MKFS_VFAT`, `_BLKID`, `_PACSTRAP`, `_ARCH_CHROOT`, `_GENFSTAB`, `_BTRDASD` | tool seams, default the bare names |

Every step prints what it did, exits 0 done, 2 cannot read or a tool failed, 3 refused (the pipeline's convention, `step_repos`). The script is bash, run as root inside the live system.

- [ ] **Step 1: Write the failing tests**

`tests/test_recovery_os_install.sh`, same shape as `tests/test_recovery_state_sync.sh` (helpers `pass`, `fail`, `check <name> <command…>`, `has <file> <needle>`, `out_has`, a `fixture` that rebuilds `$R`, stubs in `$WORK/bin` logging every call to `$LOG`, ending with `RECOVERY-OS-INSTALL SUITE GREEN`):

```bash
#!/bin/bash
# tests/test_recovery_os_install.sh — the guest-side installer against a fixture
# tree and stub tools. Nothing here mounts, formats or runs pacstrap.
set -euo pipefail
REPO=$(cd "$(dirname "$0")/.." && pwd)
SCRIPT="$REPO/scripts/recovery-os-install.sh"
WORK=$(mktemp -d); trap 'rm -rf "$WORK"' EXIT
BIN="$WORK/bin"; LOG="$WORK/calls.log"; R="$WORK/root"
FAILS=0
pass() { printf 'ok   %s\n' "$1"; }
fail() { printf 'FAIL %s\n' "$1"; FAILS=$((FAILS + 1)); }
check() { local n=$1; shift; if "$@"; then pass "$n"; else fail "$n"; fi; }
has() { grep -qF -- "$2" "$1"; }
out_has() { grep -qF -- "$1" <<<"$OUT"; }
calls() { grep -cF -- "$1" "$LOG" || :; }

# Stubs. btrfs: a subvolume is a directory under $FIX_TOP with a marker file
# .subvol holding "rw" or "ro received"; renames are the script's own mv.
mkdir -p "$BIN"
cat >"$BIN/btrfs" <<'EOF'
#!/bin/bash
echo "btrfs $*" >>"$STUB_LOG"
case "$1 $2" in
  "subvolume list") ls -1 "$FIX_TOP" | sed 's/^/ID 256 gen 1 top level 5 path /' ;;
  "subvolume show") f=$3; [[ -f "$f/.subvol" ]] || exit 1
      if grep -q received "$f/.subvol"; then printf '\tReceived UUID: \t1234-5678\n'; else printf '\tReceived UUID: \t-\n'; fi
      if grep -q ro "$f/.subvol"; then printf '\tFlags: \treadonly\n'; else printf '\tFlags: \t-\n'; fi ;;
  "subvolume create") mkdir -p "$3" && echo rw >"$3/.subvol" ;;
  "subvolume delete") [[ -z "${STUB_DELETE_FAILS:-}" ]] || exit 1; rm -rf "$3" ;;
  "property set") : ;;
  *) exit 2 ;;
esac
EOF
for t in mount umount mkfs.vfat pacstrap arch-chroot genfstab; do
  printf '#!/bin/bash\necho "%s $*" >>"$STUB_LOG"\n[[ -z "${STUB_%s_FAILS:-}" ]] || exit 1\n' "$t" "${t//[.-]/_}" >"$BIN/$t"
done
cat >"$BIN/blkid" <<'EOF'
#!/bin/bash
echo "blkid $*" >>"$STUB_LOG"; cat "$STUB_BLKID"
EOF
cat >"$BIN/btrdasd" <<'EOF'
#!/bin/bash
echo "btrdasd $*" >>"$STUB_LOG"; cat "$STUB_INSPECT"
EOF
chmod +x "$BIN"/*

fixture() {
    rm -rf "$R"; : >"$LOG"
    mkdir -p "$R/mnt/top" "$R/mnt/target" "$R/payload" "$R/etc"
    for s in @ @home @rescue @.pre-update.20261011-1016 @.half-upgraded.20261011; do
        mkdir -p "$R/mnt/top/$s"; echo rw >"$R/mnt/top/$s/.subvol"
    done
    for s in nvme/root-.20261012T0348 nvme/das-state.20261012T0348 hdd-system/ISOs.20261012T0348; do
        mkdir -p "$R/mnt/top/$s"; echo "ro received" >"$R/mnt/top/$s/.subvol"
    done
    printf '60b05268-7f8f-47b5-a38a-752576a1172a\n' >"$WORK/blkid.out"
    printf 'btrbk at boot: no\nguest agent: installed, enabled\n' >"$WORK/inspect.out"
    printf 'pkg\n' >"$R/payload/das-backup-manager-0.7.23.0-1-x86_64.pkg.tar.zst"
    printf 'ssh-ed25519 AAAA test\n' >"$R/payload/authorized_keys"
    printf '[options]\nHoldPkg = pacman\n[cachyos]\nInclude = x\n[core]\nInclude = y\n' >"$R/etc/pacman.conf"
}
run() { # <step> [VAR=value ...]
    local step=$1; shift
    RC=0
    OUT=$(env PATH="$BIN:$PATH" STUB_LOG="$LOG" FIX_TOP="$R/mnt/top" STUB_BLKID="$WORK/blkid.out" STUB_INSPECT="$WORK/inspect.out" \
        DAS_INSTALL_ROOT="$R" DAS_INSTALL_TOP="$R/mnt/top" DAS_INSTALL_TARGET="$R/mnt/target" \
        DAS_INSTALL_UUID=60b05268-7f8f-47b5-a38a-752576a1172a DAS_INSTALL_ESP_LABEL=RECOV-ESP-1 DAS_INSTALL_BAY=1 \
        DAS_INSTALL_LETTER=A DAS_INSTALL_HOSTNAME=das-recovery-bay1 DAS_INSTALL_STAMP=20261013-010203 \
        DAS_INSTALL_PACKAGE="$R/payload/das-backup-manager-0.7.23.0-1-x86_64.pkg.tar.zst" \
        DAS_INSTALL_KEYS="$R/payload/authorized_keys" \
        DAS_INSTALL_PAYLOAD_SHA256="$(sha256sum "$R/payload/das-backup-manager-0.7.23.0-1-x86_64.pkg.tar.zst" | cut -d' ' -f1)" \
        DAS_INSTALL_DEV=/dev/sdz "$@" bash "$SCRIPT" "$step" 2>&1) || RC=$?
}

echo "== replace-os: the allowlist, nothing else"
fixture; run replace-os
check "replace-os: exit 0" test "$RC" -eq 0
for s in @ @home @rescue @.pre-update.20261011-1016 @.half-upgraded.20261011; do
    check "replace-os: $s renamed to $s.replaced.20261013-010203" test -d "$R/mnt/top/$s.replaced.20261013-010203"
done
check "replace-os: a new @ exists" test -f "$R/mnt/top/@/.subvol"
check "replace-os: a new @home exists" test -f "$R/mnt/top/@home/.subvol"
for s in nvme/root-.20261012T0348 nvme/das-state.20261012T0348 hdd-system/ISOs.20261012T0348; do
    check "replace-os: received $s untouched" grep -q received "$R/mnt/top/$s/.subvol"
done
check "replace-os: nothing deleted" test "$(calls 'subvolume delete')" -eq 0
check "replace-os: p1 formatted with the bay label" has "$LOG" "mkfs.vfat -F 32 -n RECOV-ESP-1 /dev/sdz1"
check "replace-os: @ mounted at the target" has "$LOG" "mount -o subvol=/@"
check "replace-os: p1 mounted at target/boot" has "$LOG" "mount /dev/sdz1 $R/mnt/target/boot"

echo "== replace-os: the allowlist is exact (review focus 1)"
fixture
for s in @home-old @.pre-update-notes @rescue2 root-.20261012T0348; do mkdir -p "$R/mnt/top/$s"; echo "ro received" >"$R/mnt/top/$s/.subvol"; done
run replace-os
check "exact: exit 0" test "$RC" -eq 0
for s in @home-old @.pre-update-notes @rescue2 root-.20261012T0348; do
    check "exact: $s is still there, unrenamed" test -f "$R/mnt/top/$s/.subvol"
done

echo "== replace-os: --also-replace names one by one, archives only"
fixture; for s in @.archive.20260601T0300 @.archive.20260615T0300; do mkdir -p "$R/mnt/top/$s"; echo rw >"$R/mnt/top/$s/.subvol"; done
run replace-os DAS_INSTALL_ALSO_REPLACE="@.archive.20260601T0300"
check "also: exit 0" test "$RC" -eq 0
check "also: the named archive renamed" test -d "$R/mnt/top/@.archive.20260601T0300.replaced.20261013-010203"
check "also: the unnamed archive kept" test -f "$R/mnt/top/@.archive.20260615T0300/.subvol"
fixture; run replace-os DAS_INSTALL_ALSO_REPLACE="nvme"
check "also, not an archive: exit 3" test "$RC" -eq 3
check "also, not an archive: refused by name" out_has "'nvme' is not an @.archive.* subvolume -- refused"
check "also, not an archive: nothing renamed" test -f "$R/mnt/top/@/.subvol"

echo "== replace-os: wrong UUID / empty disk (review focus 2)"
fixture; printf '7c7ae72d-0000-0000-0000-000000000000\n' >"$WORK/blkid.out"; run replace-os
check "wrong uuid: exit 3" test "$RC" -eq 3
check "wrong uuid: refused before any rename" test -f "$R/mnt/top/@/.subvol"
check "wrong uuid: names both" out_has "p2 of /dev/sdz is 7c7ae72d-0000-0000-0000-000000000000, not the configured 60b05268-7f8f-47b5-a38a-752576a1172a"
fixture; : >"$WORK/blkid.out"; run replace-os
check "unreadable uuid: exit 3, never a match" test "$RC" -eq 3
fixture; rm -rf "$R/mnt/top/nvme" "$R/mnt/top/hdd-system"; run replace-os
check "empty disk: exit 3" test "$RC" -eq 3
check "empty disk: refused" out_has "p2 holds no received backup subvolume: not a recovery drive to rebuild (seed it with a backup run first) -- refused"
check "empty disk: nothing renamed" test -f "$R/mnt/top/@/.subvol"

echo "== replace-os: re-run after a partial install (review focus 5)"
fixture; run replace-os; : >"$R/mnt/top/@/half"; run replace-os DAS_INSTALL_STAMP=20261013-020000
check "re-run: exit 0" test "$RC" -eq 0
check "re-run: the partial @ set aside as @.replaced.<new stamp>" test -f "$R/mnt/top/@.replaced.20261013-020000/half"
check "re-run: the first run's replaced subvolumes still there" test -d "$R/mnt/top/@.replaced.20261013-010203"
check "re-run: a fresh @" test -f "$R/mnt/top/@/.subvol"
check "re-run: still nothing deleted" test "$(calls 'subvolume delete')" -eq 0

echo "== --count-packages"
fixture; run --count-packages
check "count: exit 0" test "$RC" -eq 0
check "count: at least the 122 of the spec plus gparted, plymouth and the meta packages" test "$OUT" -ge 125

echo "== pacstrap: the baseline, from the live system's own repositories"
fixture; run pacstrap
check "pacstrap: exit 0" test "$RC" -eq 0
check "pacstrap: -K into the target" has "$LOG" "pacstrap -K $R/mnt/target base base-devel linux-cachyos"
check "pacstrap: gparted in the baseline" has "$LOG" " gparted "
check "pacstrap: plymouth in the baseline (D9)" has "$LOG" " plymouth "
check "pacstrap: flatpak in the baseline (D6)" has "$LOG" " flatpak "
check "pacstrap: the CachyOS meta packages" has "$LOG" " cachyos-settings cachyos-hooks cachyos-mirrorlist cachyos-v3-mirrorlist cachyos-keyring"
check "pacstrap: no AUR helper" test "$(grep -c 'paru\|yay' "$LOG")" -eq 0
fixture; run pacstrap STUB_pacstrap_FAILS=1
check "pacstrap fails: exit 2" test "$RC" -eq 2
check "pacstrap fails: says so" out_has "pacstrap failed"

echo "== configure: identity, accounts, sshd, autologin, splash"
fixture; mkdir -p "$R/mnt/target/etc"; printf 'HOOKS=(base udev autodetect modconf block filesystems fsck)\n' >"$R/mnt/target/etc/mkinitcpio.conf"
run configure
check "configure: exit 0" test "$RC" -eq 0
check "configure: hostname" test "$(cat "$R/mnt/target/etc/hostname")" = das-recovery-bay1
check "configure: fstab written by genfstab -U" has "$LOG" "genfstab -U $R/mnt/target"
check "configure: pacman.conf copied from the live system" has "$R/mnt/target/etc/pacman.conf" "[cachyos]"
check "configure: locale" has "$R/mnt/target/etc/locale.conf" "LANG=en_US.UTF-8"
check "configure: keymap" has "$R/mnt/target/etc/vconsole.conf" "KEYMAP=us"
check "configure: timezone link" test "$(readlink "$R/mnt/target/etc/localtime")" = /usr/share/zoneinfo/America/Chicago
check "configure: user bosco uid 1000 in the chroot" has "$LOG" "arch-chroot $R/mnt/target useradd -m -u 1000 -G wheel -s /bin/bash bosco"
check "configure: NOPASSWD drop-in" has "$R/mnt/target/etc/sudoers.d/10-bosco" "bosco ALL=(ALL:ALL) NOPASSWD: ALL"
check "configure: sddm autologin" has "$R/mnt/target/etc/sddm.conf.d/autologin.conf" "User=bosco"
check "configure: sshd key-only (D2)" has "$R/mnt/target/etc/ssh/sshd_config.d/10-das-recovery.conf" "PasswordAuthentication no"
check "configure: sshd no root (D2)" has "$R/mnt/target/etc/ssh/sshd_config.d/10-das-recovery.conf" "PermitRootLogin no"
check "configure: sshd bosco only (D2)" has "$R/mnt/target/etc/ssh/sshd_config.d/10-das-recovery.conf" "AllowUsers bosco"
check "configure: authorized_keys copied for bosco" has "$R/mnt/target/home/bosco/.ssh/authorized_keys" "ssh-ed25519 AAAA test"
check "configure: units enabled" has "$LOG" "systemctl enable sshd NetworkManager sddm qemu-guest-agent"
check "configure: plymouth hook after udev (D9)" grep -q '^HOOKS=(base udev plymouth ' "$R/mnt/target/etc/mkinitcpio.conf"
check "configure: plymouth theme set (D9)" has "$LOG" "plymouth-set-default-theme"
check "configure: Flathub remote, no apps (D6)" has "$LOG" "flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo"
check "configure: no fstab entry for a host filesystem" test "$(grep -c '8b66e847\|d29fdda7\|20b5fa7e\|2638d087' "$R/mnt/target/etc/fstab" 2>/dev/null || :)" -eq 0

echo "== package: the project package, the marker, the unit, the masks, the desktop"
fixture; mkdir -p "$R/mnt/target/usr/lib/das-backup"
printf '[Unit]\nDescription=x\n' >"$R/mnt/target/usr/lib/das-backup/das-recovery-state-sync.service"
run package
check "package: exit 0" test "$RC" -eq 0
check "package: sha256 checked before pacman -U" out_has "package sha256 matches"
check "package: pacman -U in the chroot" has "$LOG" "arch-chroot $R/mnt/target pacman -U --noconfirm /payload/das-backup-manager-0.7.23.0-1-x86_64.pkg.tar.zst"
check "package: the payload copy removed from the target" test ! -e "$R/mnt/target/payload"
check "package: marker written" test -f "$R/mnt/target/etc/das-backup/recovery-drive"
check "package: marker names the drive" has "$R/mnt/target/etc/das-backup/recovery-drive" "label=system-recovery-A-2tb"
check "package: marker names the bay (D9 reads it)" has "$R/mnt/target/etc/das-backup/recovery-drive" "bay=1"
check "package: the sync unit installed into etc" test -f "$R/mnt/target/etc/systemd/system/das-recovery-state-sync.service"
check "package: the sync unit enabled" has "$LOG" "systemctl enable das-recovery-state-sync.service"
for u in das-backup.timer das-backup-full.timer das-scrub.timer das-backup-doctor.timer btrbk.timer btrbk.service; do
    check "package: $u masked" test "$(readlink "$R/mnt/target/etc/systemd/system/$u")" = /dev/null
done
check "package: the boot notice autostart" has "$R/mnt/target/etc/xdg/autostart/das-recovery-notice.desktop" "Exec=sh -c 'notify-send"
check "package: Quick Reference on the Desktop" test "$(readlink "$R/mnt/target/home/bosco/Desktop/Quick Reference — PRINT THIS.md")" = /usr/share/das-backup/docs/EMERGENCY-QUICK-REFERENCE.md
check "package: Recovery Guide on the Desktop" test "$(readlink "$R/mnt/target/home/bosco/Desktop/Recovery Guide.md")" = /usr/share/das-backup/docs/DISASTER-RECOVERY-GUIDE.md
check "package: Storage architecture on the Desktop" test "$(readlink "$R/mnt/target/home/bosco/Desktop/STORAGE-ARCHITECTURE-AND-RECOVERY.md")" = /usr/share/das-backup/docs/STORAGE-ARCHITECTURE-AND-RECOVERY.md
fixture; run package DAS_INSTALL_PAYLOAD_SHA256=0000
check "package, wrong sha256: exit 3" test "$RC" -eq 3
check "package, wrong sha256: pacman never ran" test "$(calls 'pacman -U')" -eq 0
fixture; run package
check "package, the package shipped no unit: exit 2" test "$RC" -eq 2
check "package, no unit: named" out_has "the package shipped no das-recovery-state-sync.service"

echo "== boot: loader.conf and the two entries, no previous-root entry yet (D1)"
fixture; run boot
check "boot: exit 0" test "$RC" -eq 0
check "boot: bootctl install in the chroot" has "$LOG" "arch-chroot $R/mnt/target bootctl install"
check "boot: loader.conf default" has "$R/mnt/target/boot/loader/loader.conf" "default cachyos-recovery.conf"
check "boot: loader.conf editor off" has "$R/mnt/target/boot/loader/loader.conf" "editor no"
check "boot: main entry title" has "$R/mnt/target/boot/loader/entries/cachyos-recovery.conf" "title CachyOS Emergency Recovery (Bay 1)"
check "boot: main entry root by UUID and subvol" has "$R/mnt/target/boot/loader/entries/cachyos-recovery.conf" "root=UUID=60b05268-7f8f-47b5-a38a-752576a1172a rw rootflags=subvol=/@ "
check "boot: main entry splash (D9)" has "$R/mnt/target/boot/loader/entries/cachyos-recovery.conf" " quiet splash"
check "boot: both microcodes" has "$R/mnt/target/boot/loader/entries/cachyos-recovery.conf" "initrd /amd-ucode.img"
check "boot: fallback entry" has "$R/mnt/target/boot/loader/entries/cachyos-recovery-fallback.conf" "Fallback (Bay 1)"
check "boot: fallback image" has "$R/mnt/target/boot/loader/entries/cachyos-recovery-fallback.conf" "initrd /initramfs-linux-cachyos-fallback.img"
check "boot: no previous-root entry on a fresh install" test ! -e "$R/mnt/target/boot/loader/entries/cachyos-recovery-previous.conf"
check "boot: mkinitcpio -P in the chroot" has "$LOG" "arch-chroot $R/mnt/target mkinitcpio -P"

echo "== verify: every file the entries name, every fstab device, btrbk at boot: no"
fixture; mkdir -p "$R/mnt/target/boot/loader/entries" "$R/mnt/target/etc"
printf 'linux /vmlinuz-linux-cachyos\ninitrd /amd-ucode.img\ninitrd /initramfs-linux-cachyos.img\n' >"$R/mnt/target/boot/loader/entries/cachyos-recovery.conf"
: >"$R/mnt/target/boot/vmlinuz-linux-cachyos"; : >"$R/mnt/target/boot/amd-ucode.img"; : >"$R/mnt/target/boot/initramfs-linux-cachyos.img"
printf 'UUID=60b05268-7f8f-47b5-a38a-752576a1172a / btrfs subvol=/@ 0 0\n' >"$R/mnt/target/etc/fstab"
run verify
check "verify: exit 0" test "$RC" -eq 0
check "verify: inspect against the new root" has "$LOG" "btrdasd recovery-os inspect --root $R/mnt/target"
check "verify: btrbk at boot: no, quoted" out_has "btrbk at boot: no"
rm "$R/mnt/target/boot/initramfs-linux-cachyos.img"; run verify
check "verify, a missing initramfs: exit 3" test "$RC" -eq 3
check "verify, a missing initramfs: named" out_has "initramfs-linux-cachyos.img named by cachyos-recovery.conf is not on the ESP"
: >"$R/mnt/target/boot/initramfs-linux-cachyos.img"; printf 'btrbk at boot: may\n' >"$WORK/inspect.out"; run verify
check "verify, btrbk may run: exit 3" test "$RC" -eq 3
check "verify, btrbk may run: the record is quoted" out_has "btrbk at boot: may"

echo "== unmount"
fixture; run unmount
check "unmount: exit 0" test "$RC" -eq 0
check "unmount: the target recursively, then the top" test "$(grep 'umount' "$LOG" | tr '\n' ' ')" = "umount -R $R/mnt/target umount $R/mnt/top "

echo "== delete-replaced: by name, after the proof (Phase 3)"
fixture; run replace-os
mkdir -p "$R/mnt/top/nvme.replaced.20261013-010203"; echo "ro received" >"$R/mnt/top/nvme.replaced.20261013-010203/.subvol"
run delete-replaced
check "delete-replaced: exit 0" test "$RC" -eq 0
for s in @ @home @rescue @.pre-update.20261011-1016 @.half-upgraded.20261011; do
    check "delete-replaced: $s.replaced.<stamp> deleted by name" has "$LOG" "subvolume delete $R/mnt/top/$s.replaced.20261013-010203"
done
check "delete-replaced: exactly five deletes" test "$(calls 'subvolume delete')" -eq 5
check "delete-replaced: a received subvolume that merely looks set-aside survives (no glob)" test -f "$R/mnt/top/nvme.replaced.20261013-010203/.subvol"
check "delete-replaced: received untouched" grep -q received "$R/mnt/top/nvme/root-.20261012T0348/.subvol"
fixture; run replace-os; run delete-replaced STUB_DELETE_FAILS=1
check "delete-replaced, a delete fails: exit 2" test "$RC" -eq 2
check "delete-replaced, a delete fails: named" out_has "could not delete @.replaced.20261013-010203"

echo "== undo: names back, refused after a new boot"
fixture; run replace-os; run undo
check "undo: exit 0" test "$RC" -eq 0
check "undo: @ is the old one again" test -f "$R/mnt/top/@/.subvol"
check "undo: the set-aside name is gone" test ! -d "$R/mnt/top/@.replaced.20261013-010203"
check "undo: the new, unbooted @ set aside as @.unbooted.<stamp>" test -d "$R/mnt/top/@.unbooted.20261013-010203"
fixture; run replace-os; mkdir -p "$R/mnt/top/@/var/lib/das-backup"; : >"$R/mnt/top/@/var/lib/das-backup/booted"; run undo
check "undo after a new boot: exit 3" test "$RC" -eq 3
check "undo after a new boot: refused" out_has "the new @ has booted (var/lib/das-backup/booted exists): undo refused"
check "undo after a new boot: nothing renamed" test -d "$R/mnt/top/@.replaced.20261013-010203"
fixture; run undo
check "undo with nothing to undo: exit 3" test "$RC" -eq 3
check "undo with nothing to undo: says so" out_has "no *.replaced.20261013-010203 subvolume"

echo
if ((FAILS > 0)); then printf '%d check(s) FAILED\n' "$FAILS"; exit 1; fi
echo "RECOVERY-OS-INSTALL SUITE GREEN"
```

The `booted` marker: the sync unit (Task 2) touches `/var/lib/das-backup/booted` on its first run on a new root; `undo` reads it from the new `@` under `$TOP`. In the fixture `@` is a directory under `$TOP`, so the path is `$TOP/@/var/lib/das-backup/booted`. The `replace-os` fixture's allowlist set has exactly five names, so "exactly five deletes" is the whole set; the sixth, `nvme.replaced.<stamp>`, is a received subvolume with a misleading name and must survive — that assertion is the one a glob would fail.

- [ ] **Step 2: Run the suite to verify it fails**

Run: `bash tests/test_recovery_os_install.sh`
Expected: every check `FAIL` (the script does not exist: `bash: …/recovery-os-install.sh: No such file`), summary `N check(s) FAILED`, exit 1.

- [ ] **Step 3: Write the installer**

`scripts/recovery-os-install.sh`:

```bash
#!/bin/bash
# recovery-os-install.sh — the guest-side installer of a DAS recovery drive's
# OS. Runs as root INSIDE the CachyOS live system, in the drive's own VM
# (recovery-os-updater-<label>), driven step by step by recovery-os-vm.sh
# provision through the guest agent. The host never writes to the drive.
# Version: 1.0.0
# Date: 2026-10-13
#
# Steps, in the order the driver runs them:
#   replace-os       p2 (by UUID) at $TOP: refuse a wrong UUID or a p2 with no
#                    received backup; rename the OS subvolumes on the allowlist
#                    to <name>.replaced.<stamp>; create @ and @home; format p1
#                    FAT32 with the bay label; mount @, @home, p1 under $TARGET
#   pacstrap         the baseline, from the live system's own repositories
#   configure        fstab, hostname, locale, tz, keymap, user, sudoers,
#                    autologin, sshd, pacman.conf, mkinitcpio, plymouth, Flathub
#   package          the project package; marker; sync unit; masks; desktop
#   boot             bootctl install, loader.conf, the entries, mkinitcpio -P
#   verify           every file the entries name; fstab devices; inspect says no
#   unmount          everything under $TARGET, then $TOP
#   delete-replaced  (Phase 3, from the NEW OS) delete <name>.replaced.<stamp>
#   undo             rename <name>.replaced.<stamp> back; refused once the new
#                    @ has booted
#   --count-packages print the baseline's package count (the driver's dry run)
# Exit: 0 done, 2 cannot read or a tool failed, 3 refused. Every parameter is
# a DAS_INSTALL_* variable; the list is in the plan
# (docs/superpowers/plans/2026-10-12-recovery-provision-plan.md, Task 1).
set -euo pipefail

STEP=${1:-}
ROOT="${DAS_INSTALL_ROOT:-}"            # test seam
TOP="${DAS_INSTALL_TOP:-$ROOT/mnt/top}"
TARGET="${DAS_INSTALL_TARGET:-$ROOT/mnt/target}"
UUID="${DAS_INSTALL_UUID:-}"
ESP_LABEL="${DAS_INSTALL_ESP_LABEL:-}"
BAY="${DAS_INSTALL_BAY:-}"
LETTER="${DAS_INSTALL_LETTER:-}"
HOST_NAME="${DAS_INSTALL_HOSTNAME:-}"
STAMP="${DAS_INSTALL_STAMP:-}"
ALSO="${DAS_INSTALL_ALSO_REPLACE:-}"
PACKAGE="${DAS_INSTALL_PACKAGE:-}"
KEYS="${DAS_INSTALL_KEYS:-}"
PKG_SHA="${DAS_INSTALL_PAYLOAD_SHA256:-}"
DEV="${DAS_INSTALL_DEV:-}"
BTRFS="${DAS_INSTALL_BTRFS:-btrfs}"; MOUNT="${DAS_INSTALL_MOUNT:-mount}"; UMOUNT="${DAS_INSTALL_UMOUNT:-umount}"
MKFS_VFAT="${DAS_INSTALL_MKFS_VFAT:-mkfs.vfat}"; BLKID="${DAS_INSTALL_BLKID:-blkid}"
PACSTRAP="${DAS_INSTALL_PACSTRAP:-pacstrap}"; CHROOT="${DAS_INSTALL_ARCH_CHROOT:-arch-chroot}"
GENFSTAB="${DAS_INSTALL_GENFSTAB:-genfstab}"; BTRDASD="${DAS_INSTALL_BTRDASD:-btrdasd}"

say() { printf '%s\n' "$*"; }
refuse() { say "$* -- refused"; exit 3; }
cannot() { say "$*"; exit 2; }
need() { local v; for v in "$@"; do [[ -n "${!v}" ]] || cannot "DAS_INSTALL_$v is not set"; done; }

# The allowlist. A name is replaced when it is exactly one of NAMES, or starts
# with one of PREFIXES, or is listed in DAS_INSTALL_ALSO_REPLACE (each of those
# must start with "@.archive."). Nothing else on the top level is touched.
NAMES=("@" "@home" "@rescue")
PREFIXES=("@.pre-update." "@.half-upgraded.")
on_allowlist() {
    local n=$1 p a
    for p in "${NAMES[@]}"; do [[ "$n" != "$p" ]] || return 0; done
    for p in "${PREFIXES[@]}"; do [[ "$n" != "$p"* ]] || return 0; done
    for a in $ALSO; do [[ "$n" != "$a" ]] || return 0; done
    return 1
}
# Top-level subvolume names: "path <name>" lines of subvolume list -o, names
# with no slash. A failed listing is a failure, never an empty top level.
top_names() {
    local out
    out=$("$BTRFS" subvolume list -o "$TOP") || cannot "cannot list the subvolumes of $TOP"
    sed -n 's/.* path //p' <<<"$out" | grep -v / || :
}
# Whether any subvolume one level down (nvme/root-.<stamp>, hdd-system/…)
# has a Received UUID: the mark of a target that received the host's backups.
received_present() {
    local d s
    for d in "$TOP"/*/; do
        [[ -d "$d" ]] || continue
        for s in "$d"*; do
            [[ -d "$s" ]] || continue
            if "$BTRFS" subvolume show "$s" 2>/dev/null | grep -Eq 'Received UUID:[[:space:]]*[0-9a-f]'; then return 0; fi
        done
    done
    return 1
}
mount_top() {
    [[ -n "$ROOT" ]] && return 0
    mkdir -p "$TOP"
    "$MOUNT" -o subvolid=5 "UUID=$UUID" "$TOP" || cannot "cannot mount p2's top level at $TOP"
}

step_replace_os() {
    need UUID ESP_LABEL STAMP DEV
    local a seen n
    for a in $ALSO; do [[ "$a" == @.archive.* ]] || refuse "'$a' is not an @.archive.* subvolume"; done
    seen=$("$BLKID" -s UUID -o value "${DEV}2") || seen=""
    [[ -n "$seen" ]] || refuse "p2 of $DEV has no readable UUID: not this drive, or not a filesystem"
    [[ "${seen,,}" == "${UUID,,}" ]] || refuse "p2 of $DEV is $seen, not the configured $UUID: not this drive"
    mount_top
    received_present || refuse "p2 holds no received backup subvolume: not a recovery drive to rebuild (seed it with a backup run first)"
    for n in $(top_names); do
        on_allowlist "$n" || continue
        [[ ! -e "$TOP/$n.replaced.$STAMP" ]] || cannot "$n.replaced.$STAMP already exists: a provision with this stamp already ran"
        say "setting aside $n as $n.replaced.$STAMP"
        mv "$TOP/$n" "$TOP/$n.replaced.$STAMP" || cannot "cannot rename $n"
    done
    "$BTRFS" subvolume create "$TOP/@" >/dev/null || cannot "cannot create @"
    "$BTRFS" subvolume create "$TOP/@home" >/dev/null || cannot "cannot create @home"
    "$MKFS_VFAT" -F 32 -n "$ESP_LABEL" "${DEV}1" || cannot "cannot format p1 as FAT32 ($ESP_LABEL)"
    mkdir -p "$TARGET"
    "$MOUNT" -o "subvol=/@,compress=zstd:3,noatime" "UUID=$UUID" "$TARGET" || cannot "cannot mount @ at $TARGET"
    mkdir -p "$TARGET/home" "$TARGET/boot"
    "$MOUNT" -o "subvol=/@home,compress=zstd:3,noatime" "UUID=$UUID" "$TARGET/home" || cannot "cannot mount @home"
    "$MOUNT" "${DEV}1" "$TARGET/boot" || cannot "cannot mount p1 at $TARGET/boot"
    say "replace-os: OS subvolumes set aside as *.replaced.$STAMP; @ and @home created; p1 formatted $ESP_LABEL; mounted at $TARGET"
}

# Spec §3.6 plus plymouth (D9) and the CachyOS meta packages the ISO's own
# installer would add. One list, both drives.
BASELINE="base base-devel linux-cachyos linux-cachyos-headers linux-firmware amd-ucode intel-ucode mkinitcpio efibootmgr efitools sbctl sudo plymouth
btrfs-progs compsize duperemove snapper e2fsprogs xfsprogs f2fs-tools exfatprogs ntfs-3g dosfstools squashfs-tools lvm2 mdadm cryptsetup nfs-utils cifs-utils smbclient
smartmontools nvme-cli hdparm hwinfo lshw dmidecode i2c-tools pciutils usbutils memtester stress iotop-c btop htop lsof strace tcpdump nmap traceroute bind
gparted partitionmanager parted gptfdisk testdisk ddrescue arch-install-scripts btrbk rsync pacman-contrib reflector flatpak
networkmanager iwd wpa_supplicant openssh openvpn wireguard-tools iproute2 curl wget
vim nano micro kate mc tmux screen bat fd ripgrep fzf jq tree python python-pip man-db man-pages texinfo util-linux
7zip tar gzip bzip2 xz zstd unzip unrar
plasma-meta sddm dolphin konsole ark filelight gwenview okular spectacle kcalc kinfocenter ksystemlog bluedevil bluez bluez-utils firefox mpv ffmpeg
cups cups-pdf print-manager system-config-printer gutenprint hplip ghostscript
qemu-guest-agent cachyos-settings cachyos-hooks cachyos-mirrorlist cachyos-v3-mirrorlist cachyos-keyring"

step_pacstrap() {
    # shellcheck disable=SC2086
    "$PACSTRAP" -K "$TARGET" $BASELINE || cannot "pacstrap failed"
    say "pacstrap: $(wc -w <<<"$BASELINE") packages into $TARGET"
}

step_configure() {
    need HOST_NAME KEYS
    local etc="$TARGET/etc"
    mkdir -p "$etc/sudoers.d" "$etc/sddm.conf.d" "$etc/ssh/sshd_config.d" "$TARGET/home/bosco/.ssh"
    "$GENFSTAB" -U "$TARGET" >"$etc/fstab" || cannot "genfstab failed"
    printf '%s\n' "$HOST_NAME" >"$etc/hostname"
    printf 'LANG=en_US.UTF-8\n' >"$etc/locale.conf"
    printf 'KEYMAP=us\n' >"$etc/vconsole.conf"
    [[ ! -f "$etc/locale.gen" ]] || sed -i 's/^#\(en_US.UTF-8 UTF-8\)/\1/' "$etc/locale.gen"
    ln -sfn /usr/share/zoneinfo/America/Chicago "$etc/localtime"
    cp "$ROOT/etc/pacman.conf" "$etc/pacman.conf" || cannot "cannot copy the live system's pacman.conf"
    "$CHROOT" "$TARGET" locale-gen >/dev/null || cannot "locale-gen failed"
    "$CHROOT" "$TARGET" useradd -m -u 1000 -G wheel -s /bin/bash bosco || cannot "useradd failed"
    printf 'bosco ALL=(ALL:ALL) NOPASSWD: ALL\n' >"$etc/sudoers.d/10-bosco"; chmod 0440 "$etc/sudoers.d/10-bosco"
    printf '[Autologin]\nUser=bosco\nSession=plasma\n' >"$etc/sddm.conf.d/autologin.conf"
    printf 'PasswordAuthentication no\nPermitRootLogin no\nKbdInteractiveAuthentication no\nAllowUsers bosco\n' >"$etc/ssh/sshd_config.d/10-das-recovery.conf"
    cp "$KEYS" "$TARGET/home/bosco/.ssh/authorized_keys" || cannot "cannot copy authorized_keys"
    chmod 0700 "$TARGET/home/bosco/.ssh"; chmod 0600 "$TARGET/home/bosco/.ssh/authorized_keys"
    "$CHROOT" "$TARGET" chown -R 1000:1000 /home/bosco || cannot "chown of /home/bosco failed"
    "$CHROOT" "$TARGET" systemctl enable sshd NetworkManager sddm qemu-guest-agent || cannot "systemctl enable failed"
    # D9: plymouth renders the sync unit's messages. The hook goes right
    # after udev (plymouth's own documented place); the theme is set here, and
    # plan 3's screenshot is the proof that the one chosen renders text.
    [[ -f "$etc/mkinitcpio.conf" ]] || cannot "no $etc/mkinitcpio.conf after pacstrap"
    grep -q '^HOOKS=.*plymouth' "$etc/mkinitcpio.conf" || sed -i 's/^HOOKS=(\(.*\)udev /HOOKS=(\1udev plymouth /' "$etc/mkinitcpio.conf"
    grep -q '^HOOKS=.*plymouth' "$etc/mkinitcpio.conf" || cannot "could not add the plymouth hook to mkinitcpio.conf (no 'udev' in HOOKS)"
    "$CHROOT" "$TARGET" plymouth-set-default-theme cachyos || "$CHROOT" "$TARGET" plymouth-set-default-theme script || cannot "no plymouth theme that renders text"
    "$CHROOT" "$TARGET" flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo || cannot "flatpak remote-add failed"
    say "configure: $HOST_NAME, bosco (uid 1000, NOPASSWD, autologin), sshd key-only, plymouth, Flathub"
}

step_package() {
    need PACKAGE PKG_SHA LETTER BAY STAMP
    local etc="$TARGET/etc" sum u
    sum=$(sha256sum "$PACKAGE" | cut -d' ' -f1) || cannot "cannot read $PACKAGE"
    [[ "$sum" == "$PKG_SHA" ]] || refuse "package sha256 is $sum, expected $PKG_SHA"
    say "package sha256 matches"
    mkdir -p "$TARGET/payload"; cp "$PACKAGE" "$TARGET/payload/" || cannot "cannot copy the package into the target"
    "$CHROOT" "$TARGET" pacman -U --noconfirm "/payload/$(basename "$PACKAGE")" || cannot "pacman -U failed"
    rm -rf "$TARGET/payload"
    mkdir -p "$etc/das-backup" "$etc/systemd/system" "$etc/xdg/autostart" "$TARGET/home/bosco/Desktop"
    printf 'label=system-recovery-%s-2tb\nbay=%s\nprovisioned=%s\n' "$LETTER" "$BAY" "$STAMP" >"$etc/das-backup/recovery-drive"
    [[ -f "$TARGET/usr/lib/das-backup/das-recovery-state-sync.service" ]] || cannot "the package shipped no das-recovery-state-sync.service"
    cp "$TARGET/usr/lib/das-backup/das-recovery-state-sync.service" "$etc/systemd/system/" || cannot "cannot install the sync unit"
    "$CHROOT" "$TARGET" systemctl enable das-recovery-state-sync.service || cannot "cannot enable the sync unit"
    for u in das-backup.timer das-backup-full.timer das-scrub.timer das-backup-doctor.timer btrbk.timer btrbk.service; do
        ln -sfn /dev/null "$etc/systemd/system/$u"
    done
    cat >"$etc/xdg/autostart/das-recovery-notice.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=DAS recovery disk: boot status
Exec=sh -c 'notify-send -t 0 "DAS recovery disk" "$(cat /run/das-recovery-state/status 2>/dev/null || echo "state sync did not run")"'
OnlyShowIn=KDE;
EOF
    ln -sfn /usr/share/das-backup/docs/EMERGENCY-QUICK-REFERENCE.md "$TARGET/home/bosco/Desktop/Quick Reference — PRINT THIS.md"
    ln -sfn /usr/share/das-backup/docs/DISASTER-RECOVERY-GUIDE.md "$TARGET/home/bosco/Desktop/Recovery Guide.md"
    ln -sfn /usr/share/das-backup/docs/STORAGE-ARCHITECTURE-AND-RECOVERY.md "$TARGET/home/bosco/Desktop/STORAGE-ARCHITECTURE-AND-RECOVERY.md"
    "$CHROOT" "$TARGET" chown -h -R 1000:1000 /home/bosco/Desktop || cannot "chown of the Desktop links failed"
    say "package: installed, marker written, sync unit enabled, 6 units masked, desktop links"
}

step_boot() {
    need UUID BAY
    local boot="$TARGET/boot" opts
    "$CHROOT" "$TARGET" bootctl install || cannot "bootctl install failed"
    mkdir -p "$boot/loader/entries"
    printf 'default cachyos-recovery.conf\ntimeout 10\nconsole-mode max\neditor no\n' >"$boot/loader/loader.conf"
    opts="root=UUID=$UUID rw rootflags=subvol=/@ quiet splash"
    printf 'title CachyOS Emergency Recovery (Bay %s)\nlinux /vmlinuz-linux-cachyos\ninitrd /amd-ucode.img\ninitrd /intel-ucode.img\ninitrd /initramfs-linux-cachyos.img\noptions %s\n' "$BAY" "$opts" >"$boot/loader/entries/cachyos-recovery.conf"
    printf 'title CachyOS Emergency Recovery — Fallback (Bay %s)\nlinux /vmlinuz-linux-cachyos\ninitrd /amd-ucode.img\ninitrd /intel-ucode.img\ninitrd /initramfs-linux-cachyos-fallback.img\noptions %s\n' "$BAY" "$opts" >"$boot/loader/entries/cachyos-recovery-fallback.conf"
    # D1: no previous-root entry on a fresh install: there is no
    # @.pre-update.* yet. The pipeline's loader step writes it after the first
    # upgrade.
    "$CHROOT" "$TARGET" mkinitcpio -P || cannot "mkinitcpio -P failed"
    say "boot: loader.conf and 2 entries written, images built"
}

step_verify() {
    local e key val f dev missing=0 rec
    for e in "$TARGET"/boot/loader/entries/*.conf; do
        [[ -f "$e" ]] || continue
        while read -r key val; do
            case "$key" in
                linux | initrd | efi)
                    f="$TARGET/boot/${val#/}"
                    [[ -f "$f" ]] || { say "${val#/} named by $(basename "$e") is not on the ESP"; missing=1; } ;;
            esac
        done <"$e"
    done
    [[ -r "$TARGET/etc/fstab" ]] || cannot "no $TARGET/etc/fstab"
    while read -r dev _; do
        case "$dev" in
            UUID=* | LABEL=* | PARTUUID=* | PARTLABEL=*)
                [[ -n "$ROOT" ]] || findfs "$dev" >/dev/null || { say "/etc/fstab names $dev, which does not resolve"; missing=1; } ;;
        esac
    done <"$TARGET/etc/fstab"
    ((missing == 0)) || refuse "verify: a file or device is missing (above)"
    rec=$("$BTRDASD" recovery-os inspect --root "$TARGET" 2>&1) || cannot "btrdasd recovery-os inspect failed: $rec"
    say "$rec"
    grep -q '^btrbk at boot: no' <<<"$rec" || refuse "the new root's boot record is not 'no'"
    say "verify: every file the entries name is on the ESP; every fstab device resolves; btrbk at boot: no"
}

step_unmount() {
    "$UMOUNT" -R "$TARGET" || cannot "cannot unmount $TARGET"
    "$UMOUNT" "$TOP" || cannot "cannot unmount $TOP"
    say "unmount: done"
}

replaced_names() { local n; for n in $(top_names); do [[ "$n" != *".replaced.$STAMP" ]] || printf '%s\n' "$n"; done; }

step_delete_replaced() {
    need UUID STAMP
    local n count=0
    mount_top
    for n in $(replaced_names); do
        # Only the names this provision set aside: an OS name (or an
        # --also-replace archive) followed by .replaced.<stamp>. A received
        # subvolume that merely carries the suffix is not one of ours.
        on_allowlist "${n%.replaced.$STAMP}" || { say "leaving $n: not a name this provision set aside"; continue; }
        "$BTRFS" property set -ts "$TOP/$n" ro false 2>/dev/null || :
        "$BTRFS" subvolume delete "$TOP/$n" || cannot "could not delete $n"
        say "deleted $n"; count=$((count + 1))
    done
    say "delete-replaced: $count subvolume(s) deleted"
}

step_undo() {
    need UUID STAMP
    local n orig any=
    mount_top
    [[ ! -e "$TOP/@/var/lib/das-backup/booted" ]] || refuse "the new @ has booted (var/lib/das-backup/booted exists): undo refused"
    for n in $(replaced_names); do
        orig=${n%.replaced.$STAMP}
        on_allowlist "$orig" || continue
        any=1
        if [[ -e "$TOP/$orig" ]]; then
            [[ "$orig" == "@" || "$orig" == "@home" ]] || cannot "$orig exists beside $n: not undone"
            mv "$TOP/$orig" "$TOP/$orig.unbooted.$STAMP" || cannot "cannot set $orig aside"
        fi
        mv "$TOP/$n" "$TOP/$orig" || cannot "cannot rename $n back"
        say "$n renamed back to $orig"
    done
    [[ -n "$any" ]] || refuse "no *.replaced.$STAMP subvolume on this filesystem: nothing to undo"
    say "undo: the old OS is back; the unbooted new root is @.unbooted.$STAMP (delete it by hand)"
}

case "$STEP" in
    replace-os) step_replace_os ;;
    pacstrap) step_pacstrap ;;
    configure) step_configure ;;
    package) step_package ;;
    boot) step_boot ;;
    verify) step_verify ;;
    unmount) step_unmount ;;
    delete-replaced) step_delete_replaced ;;
    undo) step_undo ;;
    --count-packages) wc -w <<<"$BASELINE" ;;
    *) say "usage: recovery-os-install.sh replace-os|pacstrap|configure|package|boot|verify|unmount|delete-replaced|undo|--count-packages"; exit 2 ;;
esac
```

Points the executor must keep: `on_allowlist` matches whole names and exact prefixes; the only `btrfs subvolume delete` is in `step_delete_replaced` over `replaced_names` filtered by `on_allowlist`, each printed; the `blkid` reading is split on exit status and emptiness (an unreadable p2 refuses, never "matches"); a partial `@` from an interrupted run is on the allowlist by name and becomes `@.replaced.<new stamp>`; `undo` and `delete-replaced` need `DAS_INSTALL_ALSO_REPLACE` set exactly as `replace-os` had it (the driver passes the same value to every step).

- [ ] **Step 4: Run the suite to verify it passes**

Run: `bash tests/test_recovery_os_install.sh`
Expected: every line `ok`, then `RECOVERY-OS-INSTALL SUITE GREEN`. Then `shellcheck -S warning scripts/recovery-os-install.sh tests/test_recovery_os_install.sh` → no output.

- [ ] **Step 5: Falsify the allowlist with real counter-tests**

(a) Change `on_allowlist`'s NAMES loop to `[[ "$n" != *"$p"* ]]` (substring): the "exact: @home-old …" and "exact: @rescue2 …" checks print `FAIL`. Revert. (b) Replace `replaced_names` in `step_delete_replaced` with a loop over `"$TOP"/*.replaced.*` and drop the `on_allowlist` filter: "a received subvolume that merely looks set-aside survives" prints `FAIL`. Revert. (c) Make `step_replace_os` skip the `received_present` check: "empty disk: exit 3" prints `FAIL`. Revert.

- [ ] **Step 6: Install line and ctest**

In `CMakeLists.txt` add `scripts/recovery-os-install.sh` to the `install(PROGRAMS … DESTINATION "${DAS_SCRIPT_DIR}")` list after `scripts/das-recovery-state-sync.sh`, and beside `add_test(NAME shell-recovery-state-sync …)` add:

```cmake
add_test(NAME shell-recovery-os-install
    COMMAND bash ${CMAKE_SOURCE_DIR}/tests/test_recovery_os_install.sh)
```

Run: `cmake -B /tmp/das-backup-build -DCMAKE_BUILD_TYPE=Release >/dev/null && ctest --test-dir /tmp/das-backup-build -R shell-recovery-os-install --output-on-failure` → `1/1 Test #N: shell-recovery-os-install … Passed`.

- [ ] **Step 7: Commit**

```bash
git add scripts/recovery-os-install.sh tests/test_recovery_os_install.sh CMakeLists.txt
git commit -m "recovery-os-install.sh: the guest-side installer of a recovery drive's OS (allowlist, undo, verify)"
```

---

### Task 2: D9 — the sync unit speaks on the splash, measures, and counts down

**Files:**

- Modify: `scripts/das-recovery-state-sync.sh` (1.0.2 → 1.1.0)
- Modify: `tests/test_recovery_state_sync.sh`
- Modify: `systemd/das-recovery-state-sync.service` (ordering against plymouth)

**Interfaces:**

- Consumes: the unit's existing seams and status file; the marker file is now key=value (`label=`, `bay=`, `provisioned=`, written by Task 1's `package` step).
- Produces: seams `DAS_STATE_SYNC_PLYMOUTH` (default `plymouth`), `DAS_STATE_SYNC_TIMINGS` (default `$ROOT/var/lib/das-recovery/timings` — **outside** `/var/lib/das-backup`, which the unit swaps every boot), `DAS_STATE_SYNC_BAY` (default: the `bay=` line of the marker; a marker with no such line shows "Bay ?" and says so in the journal, never refuses); the touch file `$STATE_DIR/booted` that Task 1's `undo` reads.

The splash header, verbatim from spec §3.5 D9 (`$BAY` substituted):

```
DAS recovery disk, Bay N. Recovery disk boots take significantly longer than a normal boot: this disk is a 2 TB USB drive, and it copies the latest backup index and settings before the desktop appears. Do not power off or unplug it — the copy would be left incomplete and the disk would start again from the previous copy.
```

Phase lines, each shown as the header, a blank line, then the phase text (and the countdown when measured):

| Phase key | Text |
| --- | --- |
| `find` | `finding the newest backup` |
| `config` | `preparing the configuration` |
| `copy` | `copying the backup index` |
| `done` | `ready: index from <stamp>` / `no backup received yet` / `could not update: <reason>` |

Countdown text, only when the timings file holds `total=<seconds>` from a previous boot: ` — about <N> s remaining, measured at the last boot`, N = total minus the seconds elapsed so far, floored at 0. First boot: the phase text only.

- [ ] **Step 1: Write the failing tests**

Append to `tests/test_recovery_state_sync.sh` before the summary. First extend the harness: the `run` helper passes `DAS_STATE_SYNC_PLYMOUTH="$BIN/plymouth"` and `DAS_STATE_SYNC_TIMINGS="$WORK/timings"`; add a `plymouth` stub to `$WORK/bin` that appends `plymouth $*` to `$LOG`, and for `--ping` exits 1 when `STUB_PLYMOUTH_DOWN` is set. Then:

```bash
echo "== D9: the splash says what is happening, and why"
fixture; rm -f "$WORK/timings"; run
check "d9: exit 0" test "$RC" -eq 0
check "d9: the header is the spec's sentence" has "$LOG" 'display-message --text=DAS recovery disk, Bay 1. Recovery disk boots take significantly longer than a normal boot: this disk is a 2 TB USB drive, and it copies the latest backup index and settings before the desktop appears. Do not power off or unplug it — the copy would be left incomplete and the disk would start again from the previous copy.'
check "d9: phases in order" test "$(grep -o 'finding the newest backup\|preparing the configuration\|copying the backup index\|ready: index from' "$LOG" | tr '\n' '|')" = "finding the newest backup|preparing the configuration|copying the backup index|ready: index from|"
check "d9: first boot, no countdown (review focus 4)" test "$(grep -c 'measured at the last boot' "$LOG")" -eq 0
check "d9: a measurement was written" grep -q '^total=[0-9]' "$WORK/timings"
check "d9: per-phase measurements" grep -q '^copy=[0-9]' "$WORK/timings"
check "d9: the booted touch file" test -f "$R/var/lib/das-backup/booted"

echo "== D9: the second boot counts down from the measurement"
printf 'find=1\nconfig=2\ncopy=37\ntotal=40\n' >"$WORK/timings"; : >"$LOG"; run
check "d9 second boot: exit 0" test "$RC" -eq 0
check "d9 second boot: the countdown names its measurement" grep -q 'about [0-9]* s remaining, measured at the last boot' "$LOG"
check "d9 second boot: the first phase shows the whole measured total" grep -q 'finding the newest backup — about 40 s remaining' "$LOG"
check "d9 second boot: the measurement is replaced, not appended" test "$(grep -c '^total=' "$WORK/timings")" -eq 1

echo "== D9: plymouth absent or not running costs nothing"
fixture; rm -f "$WORK/timings"; : >"$LOG"; run STUB_PLYMOUTH_DOWN=1
check "d9 no plymouth: exit 0" test "$RC" -eq 0
check "d9 no plymouth: pinged, no messages" test "$(grep -c 'display-message' "$LOG")" -eq 0
check "d9 no plymouth: still synced" status_is "synced 20261012T0348"

echo "== D9: the failure phases"
fixture; rm -rf "$R/nvme/das-state.20261012T0348"; : >"$LOG"; run
check "d9 no snapshot: exit 2" test "$RC" -eq 2
check "d9 no snapshot: said on the splash" has "$LOG" "no backup received yet"
fixture; rm -f "$WORK/timings"; : >"$LOG"; run DAS_STATE_SYNC_BTRDASD="$BIN/btrdasd-refuses"
check "d9 derive fails: said on the splash" has "$LOG" "could not update: derive-recovery refused"
check "d9 derive fails: no total written (a failed boot measures nothing)" test ! -e "$WORK/timings"

echo "== D9: a marker with no bay line"
fixture; printf 'hand-made\n' >"$R/etc/das-backup/recovery-drive"; : >"$LOG"; run
check "d9 no bay: exit 0" test "$RC" -eq 0
check "d9 no bay: Bay ?" has "$LOG" "DAS recovery disk, Bay ?."
check "d9 no bay: said in the journal" out_has "the marker has no bay= line"
```

Read the existing "derive-recovery refuses" section first and use the same knob it uses to make `derive-recovery` fail (`DAS_STATE_SYNC_BTRDASD` pointed at a refusing stub, or whatever it is); the `fixture` must write the marker as `label=system-recovery-A-2tb\nbay=1\n` from now on (update `fixture`).

- [ ] **Step 2: Run to verify they fail**

Run: `bash tests/test_recovery_state_sync.sh 2>&1 | grep -c '^FAIL'` → at least 20 (every D9 check: no `plymouth` call, no timings file, no `booted` file).

- [ ] **Step 3: Implement**

In `scripts/das-recovery-state-sync.sh`: bump `# Version: 1.1.0`, update the header's description (the splash, the measurement, the booted file), and add after the seams:

```bash
PLYMOUTH="${DAS_STATE_SYNC_PLYMOUTH:-plymouth}"
TIMINGS="${DAS_STATE_SYNC_TIMINGS:-$ROOT/var/lib/das-recovery/timings}"
BAY="${DAS_STATE_SYNC_BAY:-}"
HEADER_TEXT='Recovery disk boots take significantly longer than a normal boot: this disk is a 2 TB USB drive, and it copies the latest backup index and settings before the desktop appears. Do not power off or unplug it — the copy would be left incomplete and the disk would start again from the previous copy.'
T0=$SECONDS; PREV_TOTAL=""; SPLASH=""; DYING=""
declare -A PHASE_AT=()
# splash_init: once, after the marker gate. plymouth is spoken to only when
# it answers --ping (a bare-metal boot with no splash, or a VM without one,
# costs one failed ping and nothing else).
splash_init() {
    [[ -n "$BAY" ]] || BAY=$(sed -n 's/^bay=//p' "$MARKER" 2>/dev/null | head -1)
    [[ -n "$BAY" ]] || { BAY="?"; say "das-recovery-state-sync: the marker has no bay= line: the splash says Bay ?" >&2; }
    if "$PLYMOUTH" --ping 2>/dev/null; then SPLASH=1; fi
    PREV_TOTAL=$(sed -n 's/^total=//p' "$TIMINGS" 2>/dev/null | head -1)
    [[ "$PREV_TOTAL" =~ ^[0-9]+$ ]] || PREV_TOTAL=""
}
# phase <key> <text>: records when the phase began and shows it. The
# countdown exists only when the previous boot measured a total (D9: a
# wrong countdown is worse than none).
phase() {
    local key=$1 text=$2 left msg
    PHASE_AT[$key]=$((SECONDS - T0))
    msg="DAS recovery disk, Bay $BAY. $HEADER_TEXT"$'\n\n'"$text"
    if [[ -n "$PREV_TOTAL" ]]; then
        left=$((PREV_TOTAL - (SECONDS - T0))); ((left >= 0)) || left=0
        msg+=" — about $left s remaining, measured at the last boot"
    fi
    say "das-recovery-state-sync: $text"
    [[ -z "$SPLASH" ]] || "$PLYMOUTH" display-message --text="$msg" 2>/dev/null || :
}
# measure: every phase's seconds and the total, replacing the previous
# measurement; called only on a boot that reached "ready".
measure() {
    local now=$((SECONDS - T0)) out="" keys=(find config copy) i k next
    for ((i = 0; i < ${#keys[@]}; i++)); do
        k=${keys[$i]}; [[ -n "${PHASE_AT[$k]:-}" ]] || continue
        next=${keys[$((i + 1))]:-}
        if [[ -n "$next" && -n "${PHASE_AT[$next]:-}" ]]; then
            out+="$k=$((PHASE_AT[$next] - PHASE_AT[$k]))"$'\n'
        else
            out+="$k=$((now - PHASE_AT[$k]))"$'\n'
        fi
    done
    out+="total=$now"$'\n'
    if ! { mkdir -p "$(dirname "$TIMINGS")" && printf '%s' "$out" >"$TIMINGS.new" && mv -f "$TIMINGS.new" "$TIMINGS"; }; then
        say "das-recovery-state-sync: could not write the boot measurement to $TIMINGS" >&2
    fi
}
```

Then wire the phases: `splash_init` right after the marker gate; `phase find "finding the newest backup"` before the `newest` calls; `phase config "preparing the configuration"` before `derive-recovery`; `phase copy "copying the backup index"` before the `cp -a --reflink`; in `die()`: `[[ -n "$DYING" ]] || { DYING=1; phase done "could not update: $*"; }` before `exit 1`; at the "no state snapshot" exit: `phase done "no backup received yet"` before `exit 2`; at the end, before the final `say`: `touch "$STATE_DIR/booted" || say "… could not write $STATE_DIR/booted" >&2; measure; phase done "ready: index from $stamp"`. The `booted` file lives inside `$STATE_DIR`, which is swapped every boot: intended — it says *this root* completed a sync (undo's question), and the swap that completes recreates it in the same run.

With `set -e`, a failed `plymouth` call is `|| :`-guarded, so a dead plymouth cannot change the exit status; `phase` inside `die` runs before `exit 1`, so the status file still says `failed: …`.

In `systemd/das-recovery-state-sync.service` add to `[Unit]`: `After=plymouth-start.service` and `Before=plymouth-quit-wait.service`, so the messages are shown before the splash is torn down for sddm.

- [ ] **Step 4: Run the suite; then falsify**

Run: `bash tests/test_recovery_state_sync.sh` → `RECOVERY-STATE-SYNC SUITE GREEN`.
Falsify: (a) make `phase` always append the countdown (drop the `PREV_TOTAL` test) → "first boot, no countdown" `FAIL`; revert. (b) move `measure` above `derive-recovery` → "derive fails: no total written" `FAIL`; revert. (c) change the header's "2 TB" to "2 GB" → "the header is the spec's sentence" `FAIL`; revert.
Run `shellcheck -S warning scripts/das-recovery-state-sync.sh` → no output.

- [ ] **Step 5: Commit**

```bash
git add scripts/das-recovery-state-sync.sh tests/test_recovery_state_sync.sh systemd/das-recovery-state-sync.service
git commit -m "das-recovery-state-sync 1.1.0: the splash says what the boot is doing and why; a countdown only from the last boot's measurement (D9)"
```

---

### Task 3a: `provision` — payload, cdroms, preflight, dry run

**Files:**

- Modify: `scripts/recovery-os-vm.sh` (2.1.2 → 2.2.0): header command list (~lines 190-219), `usage_text` (709-778), knobs (~381-440), `disk_xml` (951), new functions after `cmd_screenshot` (~5493), dispatch in `main` (5533-5560)
- Modify: `tests/test_recovery_os_vm.sh` (`fixture` copies the installer beside the driver; a new section before the final `DESTROY_LOG` assertion)

**Interfaces:**

- Consumes: `load_targets`, `resolve_label`, `wait_for_lock`, `check_units`, `check_not_mounted`, `check_domain_idle`, `check_no_live_holder`, `resolve_disk` (+ `TEST_LOOP`), `make_state_dir`, `take_lock`, `start_holder`, `define_guard`, `attach_disk`, `egress_add`, `start_guarded`, `return_disk`, `write_history`, `session_exit`, `progress`, `read_history`, `resolve_os_state`.
- Produces: `cmd_provision`, `provision_parse_args`, `provision_preflight`, `provision_plan_text`, `provision_payload`, `cdrom_xml`, `attach_cdrom`, `detach_cdrom`; `disk_xml` takes an optional boot order (`${2:-1}`); knobs `DAS_RECOVERY_VM_ISO_SHA256`, `DAS_RECOVERY_VM_PAYLOAD_DIR` (default `/var/lib/libvirt/images`); `PROVISION_STAMP`; the history row gains `"mode":"provision"` and `"stamp"`.

Arguments: `provision <A|B|label> --iso <path> --package <path> --package-sha256 <hex> --authorized-keys <file> [--also-replace <name>]… [--dry-run] [--detach] [--wait-lock <min>]`, and `provision <A|B|label> --undo --iso <path>` (Task 3c).

- [ ] **Step 1: Write the failing tests**

```bash
echo "--- provision: preflight and the dry run (plan 2, Task 3a)"
provision_fixture() {
    unattended_fixture
    ISO="$T/cachyos.iso"; PKG="$T/das-backup-manager-0.7.23.0-1-x86_64.pkg.tar.zst"; KEYS="$T/keys"
    printf 'iso' >"$ISO"; printf 'pkg' >"$PKG"; printf 'ssh-ed25519 AAAA k\n' >"$KEYS"
    ISO_SHA=$(sha256sum "$ISO" | cut -d' ' -f1); PKG_SHA=$(sha256sum "$PKG" | cut -d' ' -f1)
    cat >"$T/bin/xorriso" <<'EOF'
#!/bin/bash
echo "xorriso $*" >>"$STUB/xorriso.log"
while (($# > 0)); do [[ "$1" != -o ]] || : >"$2"; shift; done
EOF
    chmod +x "$T/bin/xorriso"
    PROV=(provision A --iso "$ISO" --package "$PKG" --package-sha256 "$PKG_SHA" --authorized-keys "$KEYS")
    PROV_ENV=(DAS_RECOVERY_VM_ISO_SHA256="$ISO_SHA" DAS_RECOVERY_VM_PAYLOAD_DIR="$T/images")
}
provision_fixture
env "${PROV_ENV[@]}" true; run_driver_env() { DRIVER_EXTRA_ENV=("${PROV_ENV[@]}") run_driver "$@"; }
```

(`run_driver` must accept extra environment: add `"${DRIVER_EXTRA_ENV[@]}"` inside `driver_env`'s `env` call, defaulting to nothing. Then:)

```bash
provision_fixture
run_driver_env "${PROV[@]}" --dry-run
check "provision dry run: exit 0" "$RC" "0"
has "provision dry run: names the serial" "$OUT" "serial ZK208Q77"
has "provision dry run: the bay and hostname from display_name" "$OUT" "bay 1, hostname das-recovery-bay1, ESP label RECOV-ESP-1"
has "provision dry run: the entry titles" "$OUT" "CachyOS Emergency Recovery (Bay 1)"
matches "provision dry run: the package count" "$OUT" "packages: [0-9]+"
has "provision dry run: the ISO pinned" "$OUT" "iso sha256 ok"
has "provision dry run: the steps in order" "$OUT" "steps: egress payload capture replace-os pacstrap configure package boot repos verify unmount, then the first boot, delete-replaced"
lacks "provision dry run: nothing booted" "$(file "$S/virsh.log")" "start"
lacks "provision dry run: no payload built" "$(file "$S/xorriso.log")" "xorriso"
check "provision dry run: no capture directory (nothing changed)" "$(ls "$T/var/lib/das-backup/recovery-os-provision" 2>/dev/null | wc -l)" "0"

provision_fixture
DRIVER_EXTRA_ENV=(DAS_RECOVERY_VM_ISO_SHA256=0000) run_driver "${PROV[@]}" --dry-run
check "provision, iso sha256 mismatch: exit 1" "$RC" "1"
has "provision, iso sha256 mismatch: names both" "$OUT" "iso sha256 is $ISO_SHA, not the pinned 0000"

provision_fixture
run_driver_env "${PROV[@]}" --package-sha256 1111 --dry-run
check "provision, package sha256 mismatch: exit 1" "$RC" "1"
has "provision, package sha256 mismatch: named" "$OUT" "package sha256 is $PKG_SHA, not 1111"

provision_fixture; : >"$KEYS"
run_driver_env "${PROV[@]}" --dry-run
check "provision, empty authorized_keys: exit 1" "$RC" "1"
has "provision, empty authorized_keys: refused" "$OUT" "authorized-keys file is empty: a drive with sshd and no key is unreachable"

provision_fixture
run_driver_env "${PROV[@]}" --also-replace nvme --dry-run
check "provision, --also-replace not an archive: exit 1" "$RC" "1"
has "provision, --also-replace not an archive: refused on the host" "$OUT" "--also-replace nvme: only @.archive.* names, one per option"

provision_fixture
run_driver_env provision A --iso "$ISO" --dry-run
check "provision without --package: exit 2" "$RC" "2"

provision_fixture
printf '{"label":"system-recovery-B-2tb","start":"2026-10-12T01:00:00Z","end":"2026-10-12T02:00:00Z","mode":"unattended","unattended":true,"outcome":"failed","exit":7,"overridden":false,"kernel":null,"stopped_at":"upgrade"}\n' >>"$H"
run_driver_env "${PROV[@]}" --dry-run
check "provision, the other drive's last session failed: exit 1" "$RC" "1"
has "provision, the other drive failed: never two drives down" "$OUT" "system-recovery-B-2tb's last session was failed (stopped at upgrade): never two recovery drives down at once"

provision_fixture; rm "$T/bin/xorriso"
run_driver_env "${PROV[@]}" --dry-run
check "provision without xorriso: exit 1" "$RC" "1"
has "provision without xorriso: names the package" "$OUT" "xorriso is not installed (package libisoburn): the payload cdrom cannot be built"
```

- [ ] **Step 2: Run to verify they fail**

Run: `bash tests/test_recovery_os_vm.sh 2>&1 | grep '^FAIL' | grep -c provision` → every provision check (the driver prints `usage`, exit 2).

- [ ] **Step 3: Implement**

Knobs (beside the others, ~line 440):

```bash
ISO_SHA256="${DAS_RECOVERY_VM_ISO_SHA256:-<sha256 of cachyos-desktop-linux-260809.iso, from /hddRaid1/ISOs/cachyos-desktop-linux-260809.iso.sha256 at execution time>}"
PAYLOAD_DIR="${DAS_RECOVERY_VM_PAYLOAD_DIR:-/var/lib/libvirt/images}"
readonly PROVISION_STEP_NAMES="egress payload capture replace-os pacstrap configure package boot repos verify unmount"
```

`provision_parse_args`, mirroring `parse_session_args`: sets `P_ISO P_PKG P_PKG_SHA P_KEYS P_ALSO=() P_UNDO=false DRY_RUN DETACH WAIT_LOCK_MIN`; a missing `--iso`, or (unless `--undo`) a missing `--package`, `--package-sha256` or `--authorized-keys`, is `usage` (exit 2); each `--also-replace` value must match `^@\.archive\.[A-Za-z0-9._-]+$` or refuse `--also-replace <v>: only @.archive.* names, one per option`.

`provision_preflight`: `require_root`; `load_targets`; `resolve_label`; only a `role = mirror` target can be provisioned (refuse otherwise, naming the label); bay from `display_name` with the wipe script's regex (`[[ "$display" =~ [Bb]ay[[:space:]]*([0-9]+) ]]`; else refuse "no bay number in display_name"); `P_BAY`, `P_HOST="das-recovery-bay$P_BAY"`, `P_ESP="RECOV-ESP-$P_BAY"`, `P_LETTER` from the label (`system-recovery-A-2tb` → `A`: `[[ "$LABEL" =~ recovery-([A-Z])- ]]`, else refuse); `command -v xorriso` else refuse `xorriso is not installed (package libisoburn): the payload cdrom cannot be built`; sha256 of `--iso` against `ISO_SHA256` and of `--package` against `P_PKG_SHA`, each refusing with both values (`iso sha256 is <got>, not the pinned <want>`; `package sha256 is <got>, not <want>`); `[[ -s "$P_KEYS" ]]` else refuse `authorized-keys file is empty: a drive with sshd and no key is unreachable`; the **other** mirror target's last history row (`read_history`, last line carrying its label) must have `outcome` `clean` — none at all is a warning, not a refusal — and its OS record (`resolve_os_state` for that label, read with the record reader `check_boot_record` uses, without its refusal) must say verdict `no`, else refuse `<other>'s last session was <outcome> (stopped at <step>): never two recovery drives down at once` / `<other>'s boot record is <verdict>: never two recovery drives down at once`; then the session's own `wait_for_lock`, `check_units`, `check_not_mounted`, `check_domain_idle`, `check_no_live_holder`, `resolve_disk`, `make_state_dir`.

`provision_plan_text` prints: `serial <serial>, device <disk>`, `bay <N>, hostname <host>, ESP label <label>`, the three entry titles (`CachyOS Emergency Recovery (Bay N)`, `… — Fallback (Bay N)`, `… — Previous root, read-only (Bay N) (written by the first upgrade)`), `packages: $(bash "$(dirname "$SELF")/recovery-os-install.sh" --count-packages)`, `iso sha256 ok`, `package sha256 ok`, `also replacing: <names or none>`, `steps: <PROVISION_STEP_NAMES>, then the first boot, delete-replaced`.

`provision_payload`: `mkdir -p "$PAYLOAD_DIR"`; `PAYLOAD_ISO="$PAYLOAD_DIR/das-provision-$LABEL-$PROVISION_STAMP.iso"`; a temp dir holding `recovery-os-install.sh` (the installed copy beside this script: `$(dirname "$SELF")/recovery-os-install.sh`), the package (copied), `authorized_keys`, and `SHA256SUMS` over the three; `xorriso -as mkisofs -quiet -V DAS-PROVISION -J -R -o "$PAYLOAD_ISO" "$tmp"` or refuse; `on_exit` removes `$PAYLOAD_ISO`.

`cdrom_xml <path> <dev> <order>`:

```bash
cdrom_xml() {
    cat <<EOF
<disk type='file' device='cdrom'>
  <driver name='qemu' type='raw'/>
  <source file='$1'/>
  <target dev='$2' bus='sata'/>
  <readonly/>
  <boot order='$3'/>
</disk>
EOF
}
```

`attach_cdrom <path> <dev> <order>` / `detach_cdrom <dev>`: `virsh_ attach-device "$DOMAIN" <file> --config` / `virsh_ detach-device "$DOMAIN" <file> --config`, each proved afterwards by `disk_in_definition` (present after attach, absent after detach; a failed proof is a refusal before the next step). `disk_xml` gains `${2:-1}` for the boot order; provision uses `disk_xml "$DISK" 2` for the install boot.

`cmd_provision` (Task 3a's part): parse → `--detach` (`run_detached` with the provision arguments, same unit naming with `provision` in the name) → `check_own_unit` → preflight → `--dry-run` prints the plan and exits 0 → the rest in Task 3b.

Dispatch: `provision) cmd_provision "$@" ;;` in `main`; header and `usage_text` gain:

```
  provision <A|B|label> --iso <path> --package <path> --package-sha256 <hex>
            --authorized-keys <file> [--also-replace <@.archive.name>]...
            [--dry-run] [--detach] [--wait-lock <minutes>]
            Rebuild the drive's OS from the CachyOS live ISO inside its VM:
            the host writes nothing to the drive; p2 keeps every received
            backup; the old OS is set aside as *.replaced.<stamp> until the
            new one has booted. Exit 7 names the step it stopped at.
  provision <A|B|label> --undo --iso <path>
            Rename the set-aside OS back (refused once the new root booted).
```

- [ ] **Step 4: Run the suite**

Run: `bash tests/test_recovery_os_vm.sh >/tmp/vm-suite.log 2>&1; tail -3 /tmp/vm-suite.log` → `RECOVERY-OS-VM SUITE GREEN` (the suite takes about five minutes; run it once to a file).

- [ ] **Step 5: Falsify**

Remove the other-drive history check → "the other drive's last session failed: exit 1" `FAIL`; revert. Replace the sha256 comparison with `[[ -n "$sum" ]]` → both mismatch tests `FAIL`; revert.

- [ ] **Step 6: Commit**

```bash
git add scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh
git commit -m "recovery-os-vm.sh provision: preflight, pinned inputs, the payload cdrom and the dry run"
```

---

### Task 3b: `provision` — Phase 2 (install) and Phase 3 (first boot, proof, delete)

**Files:**

- Modify: `scripts/recovery-os-vm.sh`, `tests/test_recovery_os_vm.sh`

**Interfaces:**

- Consumes: Task 3a's `cmd_provision`, `run_stage`, `stage_output`, `wait_guest_ready`, `reboot_guest`'s guard-wait rule, `guest_sh`, `power_off_guest`, `cmd_screenshot`'s body (factor `take_screenshot <file>` out of it so both call it).
- Produces: `PROVISION_STEPS` (an array like `UNATTENDED_STEPS`), `step_prov` and the per-step functions, `provision_first_boot`, the capture written to `/var/lib/das-backup/recovery-os-provision/<label>/<stamp>/{record.json,capture.tar}`.

- [ ] **Step 1: Write the failing tests**

```bash
echo "--- provision: Phase 2 steps and Phase 3 proof (Task 3b)"
provision_fixture
run_driver_env "${PROV[@]}"
check "provision: exit 0" "$RC" "0"
check "provision: the steps in order, then the first boot's delete" "$(tr '\n' ' ' <"$S/stages.log")" "egress payload capture replace-os pacstrap configure package boot repos verify unmount delete-replaced "
has "provision: the payload cdrom attached read-only" "$(file "$S/virsh.log")" "device='cdrom'"
has "provision: the live ISO at boot order 1" "$(file "$S/virsh.log")" "<boot order='1'/>"
has "provision: the disk at boot order 2 for the install" "$(file "$S/virsh.log")" "<boot order='2'/>"
has "provision: replace-os gets the UUID" "$(file "$S/stage.replace-os.sh")" "DAS_INSTALL_UUID=60b05268-7f8f-47b5-a38a-752576a1172a"
has "provision: replace-os gets the stamp" "$(file "$S/stage.replace-os.sh")" "DAS_INSTALL_STAMP="
has "provision: delete-replaced gets the same stamp" "$(file "$S/stage.delete-replaced.sh")" "$(sed -n 's/.*DAS_INSTALL_STAMP=\([^ ]*\).*/DAS_INSTALL_STAMP=\1/p' "$S/stage.replace-os.sh" | head -1)"
has "provision: repos runs the pipeline's own check against the new root" "$(file "$S/stage.repos.sh")" "PACMAN_CONF=/mnt/target/etc/pacman.conf"
has "provision: the ISO detached before the first boot" "$(file "$S/virsh.log")" "detach-device"
has "provision: the first boot's proof" "$OUT" "PROGRESS system-recovery-A-2tb first-boot ok"
has "provision: kernel read" "$OUT" "PROGRESS system-recovery-A-2tb kernel ok"
has "provision: the sync unit's first-boot status" "$OUT" "state sync: no state snapshot (as expected before the next backup)"
has "provision: desktop proven" "$OUT" "desktop: kwin_wayland and plasmashell running; screenshot"
has "provision: the capture written" "$(ls "$T/var/lib/das-backup/recovery-os-provision/system-recovery-A-2tb/"*/ 2>/dev/null)" "capture.tar"
has "provision: the last line" "$OUT" "back in service only after the next backup run records it"
has "provision: history mode" "$(file "$H")" '"mode":"provision"'
has "provision: history stamp" "$(file "$H")" '"stamp":"'
check "provision: the payload ISO removed" "$(ls "$T/images" 2>/dev/null | wc -l)" "0"

provision_fixture; echo 2 >"$S/stage.pacstrap.rc"
run_driver_env "${PROV[@]}"
check "provision, pacstrap fails: exit 7" "$RC" "7"
has "provision, pacstrap fails: named" "$OUT" "stopped at pacstrap"
lacks "provision, pacstrap fails: nothing deleted" "$(file "$S/stages.log")" "delete-replaced"
has "provision, pacstrap fails: how to undo" "$OUT" "provision A --undo --iso"
has "provision, pacstrap fails: history failed" "$(file "$H")" '"outcome":"failed"'
has "provision, pacstrap fails: stopped_at" "$(file "$H")" '"stopped_at":"pacstrap"'

provision_fixture; : >"$S/no-reboot"
DRIVER_EXTRA_ENV=("${PROV_ENV[@]}" DAS_RECOVERY_VM_GUARD_SECS=1) run_driver "${PROV[@]}"
check "provision, no first boot: exit 7" "$RC" "7"
has "provision, no first boot: named" "$OUT" "stopped at first-boot"
lacks "provision, no first boot: nothing deleted" "$(file "$S/stages.log")" "delete-replaced"

provision_fixture; printf 'sddm\n' >"$S/guest.pgrep"
run_driver_env "${PROV[@]}"
check "provision, no desktop: exit 7" "$RC" "7"
has "provision, no desktop: named" "$OUT" "kwin_wayland or plasmashell is not running"
lacks "provision, no desktop: nothing deleted" "$(file "$S/stages.log")" "delete-replaced"

provision_fixture; printf 'synced 20261012T0348\n' >"$S/guest.sync-status"
run_driver_env "${PROV[@]}"
check "provision, an unexpected sync status: exit 7" "$RC" "7"
has "provision, an unexpected sync status: quoted" "$OUT" "state sync says 'synced 20261012T0348', expected 'no state snapshot' on a first boot"

provision_fixture; : >"$S/guard-silent-phase3"
run_driver_env "${PROV[@]}"
check "provision, the guard silent at the first boot: exit 7" "$RC" "7"
has "provision, the guard silent at the first boot: named" "$OUT" "stopped at first-boot"
provision_fixture; : >"$S/guard-silent-phase2"
run_driver_env "${PROV[@]}"
check "provision, the guard silent in the live system: not a stop" "$RC" "0"
has "provision, the guard silent in the live system: said" "$OUT" "guard: silent in the live system (expected: it has no das-vm-guard unit)"
```

Extend the virsh stub's guest-exec emulation for Phase 3's commands (`uname -r`, `systemctl is-active das-recovery-state-sync`, `cat /run/das-recovery-state/status`, `test ! -e /var/lib/das-backup/backup-index.db`, `pgrep -x kwin_wayland`, `pgrep -x plasmashell`): answer from knob files `$S/guest.<name>` (`guest.pgrep` lists the names that are running, default `kwin_wayland plasmashell`; `guest.sync-status` default `no state snapshot`), following the stub's existing `stage.$stage.*` knob style; `no-reboot` makes the domain never reset; `guard-silent-phase2`/`-phase3` make the reporter silent during the respective phase (read the stub's reporter emulation and gate it on these files).

- [ ] **Step 2: Run to verify they fail** → every Task 3b check `FAIL`.

- [ ] **Step 3: Implement**

Step scripts, each like `step_repos` (parameter lines by `printf`, then a quoted heredoc):

```bash
# Every provision step exports the installer's parameters, then runs the
# installer from the payload cdrom. $1 the installer step.
prov_env_lines() {
    printf 'export DAS_INSTALL_UUID=%s DAS_INSTALL_ESP_LABEL=%s DAS_INSTALL_BAY=%s DAS_INSTALL_LETTER=%s DAS_INSTALL_HOSTNAME=%s DAS_INSTALL_STAMP=%s DAS_INSTALL_DEV=%s\n' \
        "$MOUNT_UUID" "$P_ESP" "$P_BAY" "$P_LETTER" "$P_HOST" "$PROVISION_STAMP" /dev/sda
    printf 'export DAS_INSTALL_ALSO_REPLACE=%q DAS_INSTALL_PACKAGE=%s DAS_INSTALL_KEYS=%s DAS_INSTALL_PAYLOAD_SHA256=%s\n' \
        "${P_ALSO[*]}" "/run/media/payload/$(basename "$P_PKG")" /run/media/payload/authorized_keys "$P_PKG_SHA"
}
step_prov() { # <installer step>
    prov_env_lines
    printf 'exec /bin/bash /run/media/payload/recovery-os-install.sh %s\n' "$1"
}
step_prov_payload() {
    cat <<'EOF'
mkdir -p /run/media/payload
mount -o ro /dev/disk/by-label/DAS-PROVISION /run/media/payload || { echo "the payload cdrom (DAS-PROVISION) is not attached or not readable"; exit 2; }
cd /run/media/payload && sha256sum -c --quiet SHA256SUMS || { echo "the payload's SHA256SUMS do not match"; exit 3; }
command -v pacstrap >/dev/null || { echo "the live system has no pacstrap (arch-install-scripts): not the pinned ISO's shape"; exit 3; }
echo "payload mounted and verified; pacstrap present"
EOF
}
step_prov_capture() {
    printf 'uuid=%s\n' "$MOUNT_UUID"
    cat <<'EOF'
mkdir -p /mnt/old && mount -o ro,subvol=/@ "UUID=$uuid" /mnt/old || { echo "cannot mount the old @ read-only"; exit 2; }
cd /mnt/old && tar -cf - etc/fstab etc/pacman.conf etc/hostname etc/mkinitcpio.d etc/systemd/system boot/loader var/lib/pacman/local 2>/dev/null | base64 -w0; echo
cd / && umount /mnt/old
EOF
}
step_prov_repos() { printf 'PACMAN_CONF=%s\n' /mnt/target/etc/pacman.conf; step_repos; }
step_prov_replace_os() { step_prov replace-os; }
step_prov_pacstrap() { step_prov pacstrap; }
step_prov_configure() { step_prov configure; }
step_prov_package() { step_prov package; }
step_prov_boot() { step_prov boot; }
step_prov_verify() { step_prov verify; }
step_prov_unmount() { step_prov unmount; }
step_prov_delete_replaced() { step_prov delete-replaced; }
PROVISION_STEPS=(
    "egress 300 step_egress"
    "payload 120 step_prov_payload"
    "capture 300 step_prov_capture"
    "replace-os 300 step_prov_replace_os"
    "pacstrap 3600 step_prov_pacstrap"
    "configure 600 step_prov_configure"
    "package 600 step_prov_package"
    "boot 900 step_prov_boot"
    "repos 120 step_prov_repos"
    "verify 300 step_prov_verify"
    "unmount 120 step_prov_unmount"
)
```

The `capture` stage's output (one base64 line) is kept by `run_stage` in `STAGE_OUT` (add that: the concatenated decoded output of the stage) and written by the driver, decoded, to `…/<stamp>/capture.tar` beside `record.json` (the drive's current boot record, copied from `OS_STATE_FILE` before anything boots).

Phase 2 (in `cmd_provision` after the dry-run exit): `trap on_exit EXIT` and the signal traps → `take_lock` → `SESSION_EPOCH`, `HISTORY_ARMED=true`, `PROVISION_STAMP=$(date +%Y%m%d-%H%M%S)`, `HISTORY_MODE=provision` → `progress preflight ok "lock taken; provision $PROVISION_STAMP"` → `start_holder` → record.json → `provision_payload` → `define_guard` → `attach_disk` (order 2) → `attach_cdrom "$P_ISO" sdb 1` → `attach_cdrom "$PAYLOAD_ISO" sdc 3` → `egress_add` → `progress start start` → `start_guarded` with `GUARD_OPTIONAL=1` (the live system may have no `das-vm-guard` unit; `watch_tick` and `start_guarded` do not stop on a silent reporter while the flag is set, and the summary says `guard: silent in the live system (expected: it has no das-vm-guard unit)` or `guard: engaged in the live system`) → `wait_guest_ready` → the steps through `run_stage` exactly as `run_unattended` runs its own, with the same failure path (`progress <step> fail`, `power_off_guest`, `return_disk`, `session_exit 7` after printing `stopped at <step>; the old OS is set aside as *.replaced.$PROVISION_STAMP; provision $LABEL --undo --iso <iso> renames it back`).

Phase 3 (`provision_first_boot`): `power_off_guest "install done"` → `detach_cdrom sdb` (the live ISO; the payload stays attached, read-only: `delete-replaced` runs the installer from it) → `detach`+`attach_disk` with order 1 (proved by `disk_in_definition`) → `GUARD_OPTIONAL=` → `start_guarded` (now the guard must engage, as in `session`; a silent reporter stops it: `stopped at first-boot`) → `wait_guest_ready` → `progress first-boot start` → the proofs, each one `guest_sh`, each failure a stop naming it: `uname -r` (`progress kernel ok <r>`); `cat /run/das-recovery-state/status` must be exactly `no state snapshot` (`state sync: no state snapshot (as expected before the next backup)`; else `state sync says '<text>', expected 'no state snapshot' on a first boot`); `test ! -e /var/lib/das-backup/backup-index.db` (else `a database exists after the first boot: the sync unit copied something it should not have`); `pgrep -x kwin_wayland && pgrep -x plasmashell` (else `kwin_wayland or plasmashell is not running`); `take_screenshot "$STATE_DIR/provision-$PROVISION_STAMP-desktop.png"` (`desktop: kwin_wayland and plasmashell running; screenshot <path>`) → `progress first-boot ok` → `run_stage delete-replaced 300 "$(step_prov_delete_replaced)"` → `power_off_guest "provisioned"` → `detach_cdrom sdc` → `return_disk` → the warnings → `Provisioned $LABEL: back in service only after the next backup run records it (its record must read "btrbk at boot: no")` → `session_exit "$(session_status)"`.

`write_history` gains `"stamp": <string or null>` and reads `HISTORY_MODE` (`provision`, `undo`, or the existing values) for `mode`; `stopped_at` carries the provision step as it does for sessions.

- [ ] **Step 4: Run the suite; falsify**

Run the whole VM suite → GREEN. Falsify: skip the `pgrep` proof → "no desktop: exit 7" `FAIL`; revert. Move the `delete-replaced` stage before `progress first-boot ok` → "no desktop: nothing deleted" `FAIL`; revert. Clear `GUARD_OPTIONAL` before Phase 2's `start_guarded` → "the guard silent in the live system: not a stop" `FAIL`; revert.

- [ ] **Step 5: Commit**

```bash
git add scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh
git commit -m "recovery-os-vm.sh provision: the install steps in the live system, the first boot's proof, the set-aside OS deleted only after it"
```

---

### Task 3c: `provision --undo`

**Files:**

- Modify: `scripts/recovery-os-vm.sh`, `tests/test_recovery_os_vm.sh`

**Interfaces:**

- Consumes: Task 1's `undo` step; Task 3a's preflight (the parts that need no package: lock, claim, the drive idle, the ISO's sha256); the history's last `"mode":"provision"` row for this label (its `stamp`).
- Produces: `provision_undo`: boots the **live ISO** (the old OS cannot boot while set aside, and the new root must not boot — that is what undo prevents), runs `payload` (the payload holds only the installer) then `step_prov undo` with the stamp from the history, powers off, gives back, writes a history row `"mode":"undo"` with the same stamp. Exit 0 undone; 7 the step refused (its reason quoted); 1 no provision of this label in the history.

- [ ] **Step 1: Write the failing tests**

```bash
echo "--- provision --undo (Task 3c)"
provision_fixture
run_driver_env "${PROV[@]}"
STAMP=$(sed -n 's/.*"stamp":"\([^"]*\)".*/\1/p' "$H" | tail -1)
run_driver_env provision A --undo --iso "$ISO"
check "undo: exit 0" "$RC" "0"
has "undo: the undo step ran with the provision's stamp" "$(file "$S/stage.undo.sh")" "DAS_INSTALL_STAMP=$STAMP"
has "undo: the payload mounted first" "$(tr '\n' ' ' <"$S/stages.log")" "payload undo "
has "undo: result" "$OUT" "undone: the old OS is back"
has "undo: history" "$(file "$H")" '"mode":"undo"'
provision_fixture
run_driver_env provision A --undo --iso "$ISO"
check "undo with no provision in the history: exit 1" "$RC" "1"
has "undo with no provision: says so" "$OUT" "no provision of system-recovery-A-2tb in the history: nothing to undo"
provision_fixture
run_driver_env "${PROV[@]}"
echo 3 >"$S/stage.undo.rc"; printf 'the new @ has booted (var/lib/das-backup/booted exists): undo refused\n' >"$S/stage.undo.out"
run_driver_env provision A --undo --iso "$ISO"
check "undo refused by the installer: exit 7" "$RC" "7"
has "undo refused: the reason quoted" "$OUT" "the new @ has booted"
```

- [ ] **Step 2: Run to verify they fail** → every undo check `FAIL`.

- [ ] **Step 3: Implement** `provision_undo` per the interface (`HISTORY_MODE=undo`; the stamp read with `jq -r 'select(.label==$l and .mode=="provision") | .stamp'` over the history, last one; `P_ALSO` is re-read from that row too — add `"also_replace": [..]` to the provision row so `undo` and the installer's `on_allowlist` agree).

- [ ] **Step 4: Run the suite; falsify** (hardcode a stamp instead of reading the history → "undo with no provision: exit 1" `FAIL`; revert).

- [ ] **Step 5: Commit**

```bash
git add scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh
git commit -m "recovery-os-vm.sh provision --undo: the set-aside OS renamed back from the live system, refused once the new root booted"
```

---

### Task 4: D1 — the previous-root loader entry, rewritten by the pipeline

**Files:**

- Modify: `scripts/recovery-os-vm.sh` (`UNATTENDED_STEPS`, new `step_loader` between `initramfs` and `verify-btrbk`; header step list; the `STEPS` the suite pins)
- Modify: `tests/test_recovery_os_vm.sh` (a standalone step test like the repos one; `STEPS` updated)
- Modify: `docs/EMERGENCY-QUICK-REFERENCE.md` §How to Boot This Drive

**Interfaces:**

- Consumes: the pipeline's `snapshot` step (writes `@.pre-update.<YYYYMMDD-HHMM>`, keeps the newest two).
- Produces: `step_loader`: in the recovery OS after `initramfs`: mounts its own top level read-only at `/run/das-vm-update/top` (`subvolid=5`, by the UUID of `/`), lists `@.pre-update.*` there, reads `cachyos-recovery.conf` from the ESP (`bootctl --print-esp-path`), and writes `cachyos-recovery-previous.conf`: the same `linux`/`initrd` lines, title `<main title with "(Bay N)" replaced by "— Previous root, read-only (Bay N)">`, `options` with ` rw ` → ` ro ` and `rootflags=subvol=/@` → `rootflags=subvol=/@.pre-update.<newest>`; with no `@.pre-update.*` it removes a stale `cachyos-recovery-previous.conf` and prints `no previous root: entry absent`. Exit 0 both ways; 2 when the ESP, the main entry or the top level cannot be read. Seams: `ESP`, `TOP` (a test runs the captured script with both set to fixture directories and a `btrfs` stub on PATH).

- [ ] **Step 1: Write the failing test** — standalone like the repos test: run one unattended session to capture `stage.loader.sh`; `run_loader_script` sets `ESP=$T/esp TOP=$T/top PATH=$T/bin:$PATH` with a `btrfs` stub whose `subvolume list` prints `@`, `@.pre-update.20261011-1016`, `@.pre-update.20261012-0300` (and `mount`/`umount` stubs that log); the main entry in `$T/esp/loader/entries/cachyos-recovery.conf` as Task 1 writes it. Assert: exit 0; the previous entry exists; it names `rootflags=subvol=/@.pre-update.20261012-0300`; it has ` ro `; both `initrd` lines kept; title `CachyOS Emergency Recovery — Previous root, read-only (Bay 1)`; then an empty listing → the stale entry removed, `no previous root: entry absent`, exit 0; a missing main entry → exit 2 `cannot read cachyos-recovery.conf`. Also assert the stages log reads `… initramfs loader verify-btrbk verify-boot guard-engage ` (update `STEPS`).

- [ ] **Step 2: Run → FAIL.**

- [ ] **Step 3: Implement** `step_loader` (sh; `esp=${ESP:-$(bootctl --print-esp-path)}`; `top=${TOP:-/run/das-vm-update/top}`, mounted by the step at `subvolid=5,ro` from `findmnt -no UUID /` when `TOP` is unset, unmounted at the end; newest by `sort -r | head -1` of the `@.pre-update.*` names). Add `"loader 120 step_loader"` to `UNATTENDED_STEPS` after `initramfs`; `verify-boot` then checks the new entry's files.

- [ ] **Step 4: Suite GREEN; falsify** (`sort` instead of `sort -r` → the newest-name assertion `FAIL`; revert).

- [ ] **Step 5: Docs**: in `docs/EMERGENCY-QUICK-REFERENCE.md` §How to Boot This Drive add the third entry: "CachyOS Emergency Recovery — Previous root, read-only (Bay N): last week's known-good root, for inspection and rollback; not for daily use. Absent until the drive's first upgrade."

- [ ] **Step 6: Commit**

```bash
git add scripts/recovery-os-vm.sh tests/test_recovery_os_vm.sh docs/EMERGENCY-QUICK-REFERENCE.md
git commit -m "recovery-os-vm.sh: a loader step rewrites the previous-root entry to the newest @.pre-update after each upgrade (D1)"
```

---

### Task 5: Docs, packaging, changelog, whole-branch review, host install

**Files:**

- Modify: `docs/DISASTER-RECOVERY-GUIDE.md`: new `###` "Building a recovery drive" and "Proving a restore" under "Keeping the recovery OSes current", after "What the recovery OS knows at boot" (line ~1483); the TOC (line 11); the closing paragraph of "What the recovery OS knows at boot" (line ~1507) now points at "Building a recovery drive" for what provisioning installs and enables; a paragraph on the splash (D9) in "What the recovery OS knows at boot": the header, the phases, the countdown's provenance, `Bay ?` on a hand-made marker.
- Modify: `docs/EMERGENCY-QUICK-REFERENCE.md` §How to Boot This Drive: one line — "The screen says what the disk is doing while it copies the backup index; a countdown appears from the second boot on. Do not power off while it does."
- Modify: `CHANGELOG.md` Unreleased: Added — `recovery-os-vm.sh provision` / `--undo`, `recovery-os-install.sh`, D9 splash and measurement, D1 loader step; Changed — the marker is key=value with `bay=`, the history row has `mode: provision|undo`, `stamp`, `also_replace`; `xorriso` listed as an optional host dependency.
- Modify: `docs/ARCHITECTURE.md`: one paragraph on the provisioning flow and the two scripts.
- Modify: `.claude/rules/backup.md` §Targets and Retention: "Rebuilt by `recovery-os-vm.sh provision` (guide: Building a recovery drive); the host never writes to the drive." `.claude/rules/esp-safety.md` rule 4: add "or the CachyOS live system inside that VM during `provision`" to the permitted writers.
- Modify: `packaging/arch/PKGBUILD` `optdepends` (`libisoburn: recovery-os-vm.sh provision builds the payload cdrom`), `packaging/debian/control` Recommends (`xorriso`), `packaging/fedora/das-backup-manager.spec` Recommends (`xorriso`), as `tigervnc` is listed. CMake already installs both scripts; `grep -rn 'recovery-os-install' packaging/` must find nothing else to change.

"Building a recovery drive" must quote the `provision` and `--undo` lines verbatim from `usage_text`, list the preconditions (the other drive's last session clean and its record `no`; both sha256s; a non-empty keys file; `libisoburn` on the host; the drive seeded with at least one received backup), the sequence by phase (what each `PROGRESS` step means; the last line and what "back in service" waits for), what to do when it stops (`--undo`; the `*.replaced.<stamp>` names; the capture directory), and D7 for drive A: `--also-replace @.archive.<stamp1> --also-replace @.archive.<stamp2>`, with the two stamps left as `<stamp1>`/`<stamp2>` **in the guide's example only**, with a sentence saying plan 3 names them from the preflight capture's listing.

"Proving a restore" holds the Phase 5 procedure from spec §4 (the test subvolume and manifest, way 1 and way 2, the pass rule) and the empty table:

```markdown
| Drive | Date | Way | Files | Bytes | Duration | Result |
| --- | --- | --- | --- | --- | --- | --- |
| A | — | — | — | — | — | not yet proven |
| B | — | — | — | — | — | not yet proven |
```

- [ ] **Step 1:** write the docs; `codespell docs/ CHANGELOG.md scripts/recovery-os-install.sh scripts/recovery-os-vm.sh scripts/das-recovery-state-sync.sh` → no findings (markdownlint runs without a project config in this repo and reports MD013 on every line; codespell is the gate).
- [ ] **Step 2:** changelog, ARCHITECTURE, rules, packaging. `CMakeLists.txt`'s version stays 0.7.23.0 (D3).
- [ ] **Step 3:** `cmake --build /tmp/das-backup-build && ctest --test-dir /tmp/das-backup-build -R '^shell-' --output-on-failure` → every shell suite `Passed`; `cd indexer && CARGO_TARGET_DIR=/tmp/das-backup-target cargo test --features dbus 2>&1 | grep 'test result'` → all `0 failed` (no Rust changed; this proves nothing regressed).
- [ ] **Step 4:** commit `git commit -m "docs: building a recovery drive, proving a restore, the Last proven restore table; changelog for provision, the installer, D1 and D9"`; `~/.claude/bin/scrub-promo check origin/main..HEAD` → `CLEAN`; push; `gh run list --limit 6 --commit $(git rev-parse HEAD)` → every workflow `completed success`.
- [ ] **Step 5: Whole-branch review** by a fresh reviewer on the most capable tier (`requesting-code-review`), given the spec and this plan, asked specifically: (a) can any path reach `btrfs subvolume delete` with a name not on the allowlist or not ending in `.replaced.<stamp>`; (b) does the driver's own process `mount`, `mkfs`, `cp` or write anything to the drive (it must not); (c) is the UUID refusal before the first rename; (d) does a silent guard in Phase 3 stop the run while a silent one in Phase 2 does not; (e) is the splash header byte-identical to spec §3.5; (f) does `undo` after a partial `delete-replaced` leave the filesystem in a state the operator can read (what names remain). Fix every finding; re-run the suites; commit; push; CI green.
- [ ] **Step 6: Host install and a real dry run** under the busy check (`.claude/rules/backup.md`): `cmake --install /tmp/das-backup-build` only when no backup runs; `cmp` each installed script against the build; then as root `recovery-os-vm.sh provision A --dry-run --iso /hddRaid1/ISOs/cachyos-desktop-linux-260809.iso --package <the 0.7.23.0 package> --package-sha256 <its sha256> --authorized-keys <keys file>` prints the plan and exits 0 (or 5 if B's history has no session yet, with the warning) — nothing booted (`virsh list --all` unchanged), nothing written (the history file's line count unchanged, no payload under `/var/lib/libvirt/images`). Record the output in the session record. The rehearsal on a scratch disk is plan 3.

---

## Self-review

- **Spec coverage**: §3.1 → Task 1 `replace-os` (p1 formatted in the VM, p2 never); §3.2 allowlist and D7 → Task 1 + `--also-replace` (3a); §3.3 loader.conf and two entries → Task 1 `boot`; D1 → Task 4; §3.4 hostname, user, locale, D2 sshd → Task 1 `configure`; §3.5 package, timers masked, marker, sync unit enabled, desktop links → Task 1 `package`; D9 → Task 2 (messages, measurement, countdown) + Task 1 (plymouth in the baseline, hook, theme, `splash` on the cmdline, the autostart notice); D6 Flathub → Task 1; §3.6 baseline + gparted + plymouth → Task 1 `BASELINE`; §4 Phase 0 → Task 3a; Phase 2 → Task 3b; Phase 3 → Task 3b; `--undo` → Task 3c; Phase 4 and 5 → plan 3 (the guide text only, Task 5); §5 rows: installer ✓, provision ✓, sync unit D9 ✓, derive-recovery and the state pass (plan 1), D1 ✓, D9 ✓, guide ✓, packaging ✓. Rehearsal, screenshots, the real rebuild: plan 3.
- **Placeholders**: the ISO sha256 default and D7's two archive names are the only values not in this document; both are named as values the executor reads from the host (the `.sha256` file; the preflight capture's listing in plan 3) and the text says where. No "TBD".
- **Type consistency**: `DAS_INSTALL_*` names match between Task 1's table, its script, and Task 3b's `prov_env_lines`; the history fields `mode`/`stamp`/`also_replace` match between 3a, 3b, 3c; the step names in `PROVISION_STEPS` match the stages-log assertions in 3a's dry-run line and 3b's log line (`egress payload capture replace-os pacstrap configure package boot repos verify unmount`, then `delete-replaced`); the marker's `bay=` line is written by Task 1 and read by Task 2; the `booted` file is written by Task 2 and read by Task 1's `undo`.
- **Review focus**: 1 → Task 1 "exact"; 2 → Task 1 "empty disk"; 3 → Task 1 "undo" + Task 3c; 4 → Task 2 "first boot, no countdown" + Task 3b's first-boot proofs; 5 → Task 1 "re-run after a partial install".
- **Conflicts resolved in this plan**: the capture of the old OS is taken inside the VM from the live system, read-only, not by mounting the drive on the host (spec §2.1 wins over convenience). The live system's guard may or may not engage (systemd imports SMBIOS credentials on any system that has them; whether the pinned ISO's does is unknown until the rehearsal): Phase 2 does not depend on it, Phase 3 does — the safe direction either way, pinned by two tests. `@.partial.` was considered as a prefix for a half-filled root and dropped: `@` is already on the allowlist by name, so a re-run sets it aside as `@.replaced.<new stamp>` with no second mechanism (SI: necessity).
