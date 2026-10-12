# Recovery Drive Provisioning — Design

Status: DRAFT for operator review, 2026-10-12. Nothing in this document has been built.
Decisions marked **OPEN** need the operator's answer before the plan is written.

## 1. Why

The two 2 TB recovery drives (bay 1 `ZK208Q77` = A, bay 4 `ZFL41DNY` = B) each carry an
independent bootable OS plus the host's received backups. Both were installed on
2026-04-12 as Arch Linux with the CachyOS repositories added *behind* `[core]`/`[extra]`;
that order let Arch's `llvm-libs` 23 shadow CachyOS's mesa and killed both desktops on
2026-10-07 (bd `1igg`). The repo holds no procedure for building such a drive: nothing
installs the OS, creates the subvolumes, writes fstab or the loader entries, sets the
repository order, creates the user, or copies the guides (audit 2026-10-11). The drives
are hand-made objects; a dead one cannot be regenerated.

Operator decisions, 2026-10-11/12, that this design implements:

- The drives are **CachyOS**, respun from the verified `cachyos-desktop-linux-260809.iso`
  (`/hddRaid1/ISOs/`, sha256 and GPG verified 2026-10-11), one drive at a time; wiping is
  acceptable, the 22 TB mirrored pair holds every backup.
- Package sources, in order: **CachyOS repos → Arch repos → upstream release or Flatpak
  (interchangeable, named items only) → never AUR.** An item that exists only in AUR does
  not belong on an emergency recovery disk.
- **Primary roles: diagnostic, repair/recover, restore.** Secondary: an operational boot
  that runs on the host before anything mounts or touches the NVMe pair; a daily desktop.
- **gparted is in the baseline.**
- **btrdasd, the helper and ButteredDASD, with a current copy of the index and state, are
  on the drives**, so a restore needs no manual digging in the backup partitions.
- `@rescue` (a February 2026 second Arch root nothing can boot) is **dropped**.
- **The rebuild replaces only the OS; it never reformats a partition that holds backup data,
  on any disk** (operator, 2026-10-12). `das-wipe-all-drives-and-partition-from-scratch.sh` (renamed from
  `das-partition-drives.sh` the same day so its effect is in its name) is a fresh-enclosure
  tool that reformats every configured drive at once, the 22 TB pair included; the rebuild
  does not call it and it gains no one-drive mode. A separate mechanism replaces the OS
  subvolumes on the recovery drive's existing btrfs and reformats only its ESP, inside the VM.
- Nothing is rebuilt until this design and the plan that follows it are reviewed.

## 2. Hard constraints (not design choices)

1. **ESP ownership** (`~/.claude/rules/esp-ownership.md`, `.claude/rules/esp-safety.md`):
   only this project touches a `RECOV-ESP-*` partition, and **the host writes nothing to
   it, ever — not even a format**. The ESP is re-formatted and populated — bootloader,
   kernel, initramfs, microcode, loader entries — by the live system or the drive's own OS
   running inside its VM
   (`recovery-os-updater-<label>`), exactly as the upgrade pipeline already does.
   No ESP discovery loop anywhere: the drive is named by serial, never found by type.
2. **The recovery OS never runs btrbk, btrdasd's timers or a scrub.** Its boot record must
   read `btrbk at boot: no`. The session guard protects VM sessions; masks protect bare
   metal. The project package is installed with its timers masked at provisioning.
3. **The recovery OS boots without mounting, scanning or writing the NVMe pair** (the
   operational-boot role): no fstab entry for any host filesystem, udisks automount off,
   no ESP sync hook, no snapper for host volumes.
4. **Identify drives by serial and PARTUUID**, never by `/dev/sdX` (USB letters drift) and
   never by label alone (renameable). Labels stay as today: `RECOV-ESP-<bay>` on p1,
   `das-backup-system-recovery-<A|B>` on p2.
5. **Repository order**: the four `[cachyos*]` sections before `[core]`/`[extra]`. The
   unattended pipeline's `repos` step (f2d4804) refuses an update otherwise; provisioning
   writes the same order and runs the same check before the first boot.
6. **Pinned inputs**: ISO by sha256, project package by release tag, base image digests
   where any container is used (none planned).
7. **Possession is the credential.** Partition 2 is plain btrfs (so the host receives into
   it unattended and the OS boots with no passphrase), the OS autologs `bosco` into Plasma
   with `NOPASSWD` sudo: whoever holds the physical disk holds every backup on it and a
   root shell. sshd hardening (D2) governs only remote access to a booted drive; it is not
   data protection. Encrypting p2 is a separate decision (unlock secret at the nightly run
   and at the firmware menu) and is out of this rebuild's scope unless the operator asks.
8. **Build nothing from AUR**: no AUR helper, no `base-devel`-driven builds on the drives.
   (`base-devel` stays in the baseline because `arch-install-scripts` and chroot repair
   work need it; it is not an AUR enabler by itself.)

## 3. What a rebuilt drive looks like

### 3.1 Partition layout (unchanged; the partition table is not touched)

| Partition | Size | FS | Label | Rebuild action |
| --- | --- | --- | --- | --- |
| p1 | 1.5 GiB | FAT32 | `RECOV-ESP-<bay>` | **re-formatted inside the VM** by the live system (FAT32, bay label), then populated by the new OS in its chroot |
| p2 | rest | btrfs | `das-backup-system-recovery-<A\|B>` | **never formatted**; the OS subvolumes on it are replaced, every received backup subvolume stays |

No partition table is written, no btrfs filesystem is made, and no drive other than the one
lent to the VM is reachable from the live system (the VM has exactly that one disk and
the ISO). p2 is identified by the `mount_uuid` `config.toml` names for the target, as
every session is; a p2 whose UUID differs is a refusal before anything is written.
`das-wipe-all-drives-and-partition-from-scratch.sh` remains the fresh-enclosure tool and is not used.

### 3.2 Subvolumes on p2 (top level)

| Subvolume | Role |
| --- | --- |
| `@` | the OS root; `rootflags=subvol=/@` |
| `@home` | home; separate so a root rollback keeps the user's files |
| `@.pre-update.<stamp>` | written by the upgrade pipeline, newest two kept (exists today) |
| received backup subvolumes | written by btrbk from the host, as today; **re-seeded by the first run after the rebuild** |

**OS subvolumes replaced** (an explicit allowlist of names; nothing else is touched):
`@`, `@home`, `@.pre-update.*`, `@.half-upgraded.*`, `@rescue`. Each is renamed to
`<name>.replaced.<stamp>` first and deleted only after Phase 3's first boot succeeded, so a
failed provision can be undone by renaming back (the old OS is intact until then).
**Everything not on that list is kept untouched**: every received backup subvolume.
**D7 (decided 2026-10-12)**: the two `@.archive.*` boot archives on A (June 2026, left for
manual review by the pruner's rule) are **deleted** during A's rebuild — named one by one in
the plan, never by a glob; `@.archive.*` joins the allowlist for A only.

### 3.3 Boot (systemd-boot, written from inside the VM)

`loader.conf`: `default cachyos-recovery.conf`, `timeout 10`, `console-mode max`, `editor no`.

Entries, titles as today's drives have them (`docs/EMERGENCY-QUICK-REFERENCE.md`):

- `cachyos-recovery.conf` — "CachyOS Emergency Recovery (Bay N)", kernel `linux-cachyos`,
  `amd-ucode` + `intel-ucode` + default initramfs, `root=UUID=<p2> rw rootflags=subvol=/@`.
- `cachyos-recovery-fallback.conf` — "… — Fallback (Bay N)", the fallback initramfs.
- **D1 (accepted 2026-10-12)** `cachyos-recovery-previous.conf` — "… — Previous root, read-only (Bay N)":
  `rootflags=subvol=/@.pre-update.<newest>`, rewritten by the pipeline after each
  successful upgrade. Boots last week's known-good root for inspection and rollback
  without external media (what `@rescue` was for, with zero maintenance). Read-only root:
  for inspection, not daily use.

`mkinitcpio` preset `linux-cachyos` with `default` and `fallback` images (the pipeline's
`initramfs` step already adds `fallback` to a preset lacking it).

### 3.4 Identity and accounts

- Hostname `das-recovery-bay<N>` **derived from the bay in `config.toml`'s display name**,
  the way the ESP label is (B still says `bay3` today, stale since 2026-05-06).
- User `bosco` uid 1000, `NOPASSWD: ALL` sudoers drop-in, SDDM autologin to Plasma (as today).
- Locale `en_US.UTF-8`, timezone `America/Chicago`, keymap `us` (as today).
- **OPEN D2** `sshd` on both drives, key-only (`PasswordAuthentication no`,
  `PermitRootLogin no`, `KbdInteractiveAuthentication no`), the operator's key and the
  project's `~/.claude/ssh/id_ed25519.pub` authorised for `bosco` (no other account). The
  VM path never needs it (guest agent); it serves bare-metal use and the Phase 5 bare-metal
  restore proof. Or: the operator's key only, or off. (B has sshd enabled today; A does not.)

### 3.5 The project's own tooling on the drive (operator decision)

- **Package**: `das-backup-manager` built from `packaging/arch/PKGBUILD` at a **pinned
  release tag** and installed with pacman inside the chroot. The tag must exist as a
  GitHub release at rebuild time. **D3 (decided 2026-10-12)**: the rebuild pins **`0.7.23.0`**,
  which stays the *staged* release while the provisioning work (state-sync unit,
  recovery-mode config, guest installer, previous-root entry — all package content a drive
  needs) rolls into it; it is promoted to GitHub only after that work is finished, with the
  `.staged-release` breadcrumb advanced to the final commit. The published tag stays
  `0.7.22.3` until then. Order: finish the code → promote 0.7.23.0 → rehearse against it →
  rebuild.
- **Timers masked**: `das-backup.timer`, `das-backup-full.timer`, `das-scrub.timer`,
  `das-backup-doctor.timer`, and `btrbk.timer`/`btrbk.service`, so the boot record reads
  `btrbk at boot: no`.
- **State sync at boot** — `das-recovery-state-sync.service`, before `sddm`: find the
  newest received `@.<stamp>` snapshot on the drive's own p2, reflink-copy its
  `var/lib/das-backup/` into the recovery OS's `/var/lib/das-backup/` (same btrfs: instant,
  no space until either side changes). Loud on failure (journal + a desktop notification
  file the guide names), and **leaves the previous copy in place — never an empty
  database**. Two-direction test: a fixture with a snapshot → copied; no snapshot /
  unreadable p2 → refused, previous copy intact.
- **Recovery-mode configuration — OPEN D4.** The host's `config.toml` names the host's
  sources and the three targets by mount point. On a recovery boot the only target that
  exists is this drive's own p2. Options: (a) provisioning writes
  `/etc/das-backup/config.toml` tuned for recovery (one target = this drive's p2 at
  `/mnt/backup-system-recovery-<A|B>`, no sources, email off); (b) `btrdasd` grows
  `--recovery` that derives (a) from the drive it booted from. Recommendation: (a) now,
  because it ships with the procedure and is testable with the existing config validator;
  (b) later if drift appears.
- **GUI**: `btrdasd-gui` installed; desktop file present; the two guides symlinked on the
  Desktop as today ("Quick Reference — PRINT THIS.md", "Recovery Guide.md" →
  `/usr/share/das-backup/docs/…`), plus `STORAGE-ARCHITECTURE-AND-RECOVERY.md`.

### 3.6 Package baseline (one list, both drives)

Union of both drives' explicit lists on 2026-10-12 (identical sets, 122 names; B marks the
five compressors explicit) **plus** `gparted` **plus** `das-backup-manager`. Grouped by role so
a reviewer sees why each is there. CachyOS meta packages come from the ISO's own install
(`cachyos-settings`, `cachyos-hooks`, `cachyos-mirrorlist`, `cachyos-v3-mirrorlist`,
`cachyos-keyring`).

| Role | Packages |
| --- | --- |
| base and boot | `base base-devel linux-cachyos linux-cachyos-headers linux-firmware amd-ucode intel-ucode mkinitcpio efibootmgr efitools sbctl sudo` |
| filesystems | `btrfs-progs compsize duperemove snapper e2fsprogs xfsprogs f2fs-tools exfatprogs ntfs-3g dosfstools squashfs-tools lvm2 mdadm cryptsetup nfs-utils cifs-utils smbclient` |
| diagnostic and analysis | `smartmontools nvme-cli hdparm hwinfo lshw dmidecode i2c-tools pciutils usbutils memtester stress iotop-c btop htop lsof strace tcpdump nmap traceroute bind` |
| repair and recovery | `gparted partitionmanager parted gptfdisk testdisk ddrescue arch-install-scripts btrbk das-backup-manager rsync pacman-contrib reflector` |
| network | `networkmanager iwd wpa_supplicant openssh openvpn wireguard-tools iproute2 curl wget` |
| shell and editors | `vim nano micro kate mc tmux screen bat fd ripgrep fzf jq tree python python-pip man-db man-pages texinfo util-linux` |
| archives | `7zip tar gzip bzip2 xz zstd unzip unrar` |
| desktop | `plasma-meta sddm dolphin konsole ark filelight gwenview okular spectacle kcalc kinfocenter ksystemlog bluedevil bluez bluez-utils firefox mpv ffmpeg` |
| printing (as today) | `cups cups-pdf print-manager system-config-printer gutenprint hplip ghostscript` |
| VM | `qemu-guest-agent` |

**OPEN D5**: printing stays (it is on both drives today) or goes. **OPEN D6**: Flatpak
runtime preinstalled (`flatpak` + Flathub remote, no apps) or not.

## 4. How a drive is rebuilt — the mechanism

All steps are driven from the host by **one new subcommand of the existing driver**,
`recovery-os-vm.sh provision <A|B|label> --iso <path> --package <path>`, because the
driver already owns everything the job needs: the maintenance lock, the exclusive disk
claim, the per-drive VM with its guard, the egress rule, guest-agent execution, machine
lines (`PROGRESS`/`OUTPUT`/`RESULT`), the history, and the own-unit refusal.

### Phase 0 — preflight (host, nothing changed)

- Backup, scrub and doctor units inactive; maintenance lock taken; the drive claimed.
- The other recovery drive's last session `clean` and its boot record `no` (never two
  drives down at once).
- ISO sha256 matches the pinned value; the package file's sha256 matches its release
  asset; both readable.
- A fresh **capture** of the drive about to be wiped (`pacman -Qqe`, fstab, loader
  entries, presets, pacman.conf, enabled units, users, desktop) written to
  `/var/lib/das-backup/recovery-os-provision/<label>/<stamp>/` — the last record of what
  was there.
- A `--dry-run` prints the plan: serial, device, labels, hostname, bay, entry titles,
  package count, and exits.

### Phase 1 — nothing on the host

- The host writes nothing to the drive. There is no partitioning phase: the partition table,
  p2's filesystem and every received backup subvolume survive the rebuild, so the first
  backup run afterwards is an **ordinary incremental**, not a full send. The SMART self-test
  gate of the wipe tool is reproduced in preflight (same rule: the newest self-test PASSED,
  none running), since a drive about to be rewritten should be healthy.

### Phase 2 — install (inside the drive's VM, from the live ISO)

- The driver defines the VM as for a session, attaches p2's whole disk as SATA (boot
  order 2) and the ISO as a read-only SATA cdrom with **boot order 1**, and boots. The
  live CachyOS system carries `qemu-guest-agent` and `arch-install-scripts` (verified
  2026-10-11), so the driver runs the install through `guest-exec` exactly as the upgrade
  steps run, each step a transient unit in the live system with its output and exit
  status polled (the ea3u-hardened probe).
- Steps (each a function writing a `sh` script, like `step_egress`): `egress` (the same
  check), `replace-os` (p2 by `mount_uuid` at
  `/mnt/top` with `subvolid=5`; refuse if its UUID is not the configured one or if it holds
  no received backup subvolume — a wrong or empty disk; rename the allowlisted OS
  subvolumes to `*.replaced.<stamp>`; create `@` and `@home`; format p1 FAT32 with the bay
  label; mount `@` at `/mnt/target`, `@home` and p1 under it), `pacstrap` (the live system's own CachyOS repo order, the
  baseline list), `configure` (fstab by UUID, hostname, locale, tz, keymap, user, sudoers,
  autologin, pacman.conf order check, mkinitcpio preset), `package` (`pacman -U` the
  project package in the chroot, mask the timers, write the recovery-mode config, the
  state-sync unit, the desktop symlinks, `das-vm-guard` as the pipeline expects it),
  `boot` (`bootctl install` in the chroot, loader.conf, the entries), `repos` (the same
  check as the pipeline, against the new root's pacman.conf), `verify` (every file the
  entries name is on the ESP; every fstab device resolves; `btrdasd recovery-os inspect`
  against the new root says `btrbk at boot: no` and names the agent), `unmount`, power off.
- Anything unexpected stops it the way the pipeline stops: the live system is powered
  off, the disk given back, exit 7 naming the step. A stopped provision leaves the
  `*.replaced.<stamp>` subvolumes in place and the backups untouched; `provision --undo
  <label>` renames them back (refusing if a new `@` booted since); `provision` is
  re-runnable.

### Phase 3 — first boot and proof (inside the VM, from the disk)

- The driver reboots the VM with the ISO detached, boot order disk first, under the
  session guard. It must come back with the guard engaged and the agent answering (the
  pipeline's `reboot` step), then: `uname -r` is the installed kernel; the state-sync
  unit reports no snapshot yet and left no database (the "no snapshot" direction of its
  test, observed on a real drive); a screenshot shows the Plasma desktop; `kwin_wayland`
  and `plasmashell` are running. Then, and only then, the `*.replaced.<stamp>` subvolumes are
  deleted (inside the VM, by the new OS, named one by one — never a glob over the top
  level). Power off, give back.

### Phase 4 — the next backup run

- The next nightly run (or `backup-run.sh` by hand) sends its ordinary incrementals to the
  untouched received subvolumes, records the new OS (a `no` record with the agent
  installed), and the following session is admitted. Only then is the drive back in
  service; the guide says so, and `provision` prints it as its last line.

Then the second drive, only after the first one's run completed and its record reads `no`.

### Phase 5 — restore proof (operator requirement, 2026-10-12: "the only good backup is a backup that has been successfully restored")

After **both** drives are rebuilt and re-seeded, each drive proves a real restore, two ways:

- **Test data**: a dedicated subvolume on the host (`@restore-test`, under a source that
  fans out to all three targets) populated with unimportant files of mixed sizes
  (including a sparse file, a hard link, a symlink, a file with a space in its name, and
  one over 4 GiB), with a `sha256sum` manifest written beside it; backed up by an
  ordinary run so the recovery drives hold it as a received snapshot.
- **Way 1 — in the VM**: an attended session of the drive with a **scratch disk image on
  the NVMe** (`/hddRaid1`-free: `/var/lib/libvirt/images/restore-test-<label>.qcow2`)
  attached as a second SATA disk; inside the recovery OS, restore the test subvolume from
  the drive's own received snapshot onto that disk (btrdasd restore with the recovery-mode
  config, destination on the scratch disk). The VM cannot see the host's root pool; the
  data still crosses from the recovery drive to NVMe-backed storage.
- **Way 2 — bare metal (BMR mode)**: boot the drive on the real hardware from the
  firmware menu, restore the same subvolume onto the host's NVMe under a root the restore
  policy allows (`[restore] allowed_roots`: `/home/restore-test/`), **through
  ButteredDASD** for at least one of the two drives — the path a stressed person would
  use. Afterwards run a backup with the drive attached before its next VM session (a
  bare-metal boot leaves no session record; the guide's rule).
- **Pass**: for all four restores, `sha256sum -c` against the manifest reports every file
  OK, file count and sizes match, and the restore's own report says it restored
  everything. Any mismatch fails the rebuild of that drive. Results go in the guide's
  "Last proven restore" table (drive, date, way, files, bytes, duration) and in the
  session history.
- The test subvolume and `/home/restore-test/` are deleted after the proof; the scratch
  images are deleted with their libvirt definitions.

## 5. Components to build (the plan's tasks come from this list)

| Component | Where | Tests |
| --- | --- | --- |
| `scripts/recovery-os-install.sh` — the guest-side installer (runs in the live system; `replace-os`, `pacstrap`, `configure`, `package`, `boot`, `repos`, `verify`, `unmount`, `undo`) | new file, installed under `/usr/lib/das-backup/` and shipped into the VM by the driver | `tests/test_recovery_os_install.sh` against a fixture tree and stub tools: the allowlist (a received subvolume is never renamed or deleted), the UUID refusal, the empty-disk refusal, `undo` refusing after a new boot, every step's failure direction |
| `provision` subcommand, steps, dry run, history row | `scripts/recovery-os-vm.sh` | `tests/test_recovery_os_vm.sh` stubs (new section): step order, every refusal, the generated step scripts run standalone against stub tools as the `repos`/`egress` tests do |
| `das-recovery-state-sync.service` + script | `config/` or `scripts/`, installed by the package | a bash test with a fixture btrfs-like tree: copies from the newest snapshot; no snapshot / unreadable → refuses, previous copy intact |
| recovery-mode `config.toml` template | `config/recovery-config.toml` | `btrdasd config validate` in CI against it |
| previous-root loader entry (D1) | the pipeline's `verify-boot`/a new `loader` step | entry rewritten after a clean upgrade; absent when no snapshot |
| guide section "Building a recovery drive" + "Proving a restore" + "Last proven restore" table + quick-reference line | `docs/DISASTER-RECOVERY-GUIDE.md`, `docs/EMERGENCY-QUICK-REFERENCE.md` | markdownlint; the install and session lines quoted verbatim from the script's usage |
| packaging | every format in `packaging/` gets the new unit and template | the existing packaging checks |

**Rehearsal before any real drive**: the whole `provision` flow runs first against a
**scratch qcow2 disk** attached to `test-vm-cachyos` in place of a recovery drive (the
driver already accepts a test loop device; a loop/qcow2 path is the same shape), with the
real ISO and package. The drive gets rebuilt only after the rehearsal produced a bootable
desktop and a `no` record. That is the falsification of the procedure itself.

## 6. Risks and what bounds them

| Risk | Bound |
| --- | --- |
| wrong drive touched | the VM holds exactly the one disk the session lent it, by serial; p2 is refused unless its UUID is the configured target's and it holds received backups; no partition table and no btrfs format anywhere in the flow |
| backup data lost on the drive | nothing on p2 is deleted except names on the OS allowlist, and those only after the new OS booted; `*.replaced.<stamp>` keeps the old OS until then; the suite's falsification renames a received subvolume to look like an OS one and watches the allowlist refuse it |
| both recovery drives unusable at once | preflight refuses unless the other drive's last session is `clean` and its record `no` |
| host writes the ESP | the host only formats it; every write is a guest-exec inside the VM; the guard's bind of btrbk is the same mechanism the pipeline trusts |
| the live ISO changes shape (no agent, different tools) | ISO pinned by sha256; preflight checks `qemu-guest-agent` and `pacstrap` exist in the live system before Phase 1 |
| the next run is a full send after all | it is not: received subvolumes are untouched; the first run's report is checked for incremental sends, and a full send is a finding, not a plan |
| the state-sync leaves a stale index | by design it is as current as the last run; the unit prints the snapshot's date on the desktop notification and `btrdasd health` shows it |
| timers run on the recovery OS | masked at provisioning; `btrdasd recovery-os inspect` in Phase 2 and the nightly record both must read `no`, or the procedure fails |

## 7. Out of scope

Reworking `backup-run.sh`'s send order or retention; a host-side NVMe installer; any
change to the 22 TB primary; Secure Boot signing (sbctl is installed for repair work only).

## 8. Open decisions, collected

| Id | Question | Recommendation |
| --- | --- | --- |
| D1 | previous-root loader entry | **accepted** |
| D2 | sshd on the drives | on, key-only, both drives |
| D3 | package release to pin | **0.7.23.0, staged until the work is done, then promoted** |
| D4 | recovery-mode config: template vs `btrdasd --recovery` | template now |
| D5 | printing stack | keep (already there; harmless) |
| D6 | Flatpak runtime preinstalled | no; add on a named need |
| D7 | the two `@.archive.*` boot archives on A | **delete** (operator) |
