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
    printf '5eed0008-0000-4000-8000-000000000008\n' >"$WORK/blkid.out"
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
        DAS_INSTALL_UUID=5eed0008-0000-4000-8000-000000000008 DAS_INSTALL_ESP_LABEL=RECOV-ESP-1 DAS_INSTALL_BAY=1 \
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
for s in @home-old @.pre-update-notes @rescue2 root-.20261012T0348 old@.pre-update.20261001 @x.half-upgraded.20261001; do mkdir -p "$R/mnt/top/$s"; echo "ro received" >"$R/mnt/top/$s/.subvol"; done
run replace-os
check "exact: exit 0" test "$RC" -eq 0
for s in @home-old @.pre-update-notes @rescue2 root-.20261012T0348 old@.pre-update.20261001 @x.half-upgraded.20261001; do
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
fixture; printf '5eed0010-0000-0000-0000-000000000000\n' >"$WORK/blkid.out"; run replace-os
check "wrong uuid: exit 3" test "$RC" -eq 3
check "wrong uuid: refused before any rename" test -f "$R/mnt/top/@/.subvol"
check "wrong uuid: names both" out_has "p2 of /dev/sdz is 5eed0010-0000-0000-0000-000000000000, not the configured 5eed0008-0000-4000-8000-000000000008"
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
fixture; run replace-os; run replace-os
check "same stamp twice: exit 2, never overwrites a set-aside name" test "$RC" -eq 2
check "same stamp twice: named" out_has "@.replaced.20261013-010203 already exists"

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
check "configure: user user uid 1000 in the chroot" has "$LOG" "arch-chroot $R/mnt/target useradd -m -u 1000 -G wheel -s /bin/bash user"
check "configure: NOPASSWD drop-in" has "$R/mnt/target/etc/sudoers.d/10-user" "user ALL=(ALL:ALL) NOPASSWD: ALL"
check "configure: sddm autologin" has "$R/mnt/target/etc/sddm.conf.d/autologin.conf" "User=user"
check "configure: sshd key-only (D2)" has "$R/mnt/target/etc/ssh/sshd_config.d/10-das-recovery.conf" "PasswordAuthentication no"
check "configure: sshd no root (D2)" has "$R/mnt/target/etc/ssh/sshd_config.d/10-das-recovery.conf" "PermitRootLogin no"
check "configure: sshd user only (D2)" has "$R/mnt/target/etc/ssh/sshd_config.d/10-das-recovery.conf" "AllowUsers user"
check "configure: authorized_keys copied for user" has "$R/mnt/target/home/user/.ssh/authorized_keys" "ssh-ed25519 AAAA test"
check "configure: units enabled" has "$LOG" "systemctl enable sshd NetworkManager sddm qemu-guest-agent"
check "configure: plymouth hook after udev (D9)" grep -q '^HOOKS=(base udev plymouth ' "$R/mnt/target/etc/mkinitcpio.conf"
check "configure: plymouth theme set (D9)" has "$LOG" "plymouth-set-default-theme"
check "configure: Flathub remote, no apps (D6)" has "$LOG" "flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo"
check "configure: no fstab entry for a host filesystem" test "$(grep -c '5eed0011\|5eed0020\|5eed0003\|5eed0004' "$R/mnt/target/etc/fstab" 2>/dev/null || :)" -eq 0

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
check "package: Quick Reference on the Desktop" test "$(readlink "$R/mnt/target/home/user/Desktop/Quick Reference — PRINT THIS.md")" = /usr/share/das-backup/docs/EMERGENCY-QUICK-REFERENCE.md
check "package: Recovery Guide on the Desktop" test "$(readlink "$R/mnt/target/home/user/Desktop/Recovery Guide.md")" = /usr/share/das-backup/docs/DISASTER-RECOVERY-GUIDE.md
check "package: Storage architecture on the Desktop" test "$(readlink "$R/mnt/target/home/user/Desktop/STORAGE-ARCHITECTURE-AND-RECOVERY.md")" = /usr/share/das-backup/docs/STORAGE-ARCHITECTURE-AND-RECOVERY.md
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
check "boot: main entry root by UUID and subvol" has "$R/mnt/target/boot/loader/entries/cachyos-recovery.conf" "root=UUID=5eed0008-0000-4000-8000-000000000008 rw rootflags=subvol=/@ "
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
printf 'UUID=5eed0008-0000-4000-8000-000000000008 / btrfs subvol=/@ 0 0\n' >"$R/mnt/target/etc/fstab"
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
