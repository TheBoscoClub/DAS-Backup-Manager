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
    local d s info
    for d in "$TOP"/*/; do
        [[ -d "$d" ]] || continue
        for s in "$d"*; do
            [[ -d "$s" ]] || continue
            # Capture first: `show | grep -q` dies of SIGPIPE under pipefail
            # when grep matches early, and a failed pipeline reads as "no".
            info=$("$BTRFS" subvolume show "$s" 2>/dev/null) || continue
            if grep -Eq 'Received UUID:[[:space:]]*[0-9a-f]' <<<"$info"; then return 0; fi
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
    mkdir -p "$etc/sudoers.d" "$etc/sddm.conf.d" "$etc/ssh/sshd_config.d" "$TARGET/home/user/.ssh"
    "$GENFSTAB" -U "$TARGET" >"$etc/fstab" || cannot "genfstab failed"
    printf '%s\n' "$HOST_NAME" >"$etc/hostname"
    printf 'LANG=en_US.UTF-8\n' >"$etc/locale.conf"
    printf 'KEYMAP=us\n' >"$etc/vconsole.conf"
    [[ ! -f "$etc/locale.gen" ]] || sed -i 's/^#\(en_US.UTF-8 UTF-8\)/\1/' "$etc/locale.gen"
    ln -sfn /usr/share/zoneinfo/America/Chicago "$etc/localtime"
    cp "$ROOT/etc/pacman.conf" "$etc/pacman.conf" || cannot "cannot copy the live system's pacman.conf"
    "$CHROOT" "$TARGET" locale-gen >/dev/null || cannot "locale-gen failed"
    "$CHROOT" "$TARGET" useradd -m -u 1000 -G wheel -s /bin/bash user || cannot "useradd failed"
    printf 'user ALL=(ALL:ALL) NOPASSWD: ALL\n' >"$etc/sudoers.d/10-user"; chmod 0440 "$etc/sudoers.d/10-user"
    printf '[Autologin]\nUser=user\nSession=plasma\n' >"$etc/sddm.conf.d/autologin.conf"
    printf 'PasswordAuthentication no\nPermitRootLogin no\nKbdInteractiveAuthentication no\nAllowUsers user\n' >"$etc/ssh/sshd_config.d/10-das-recovery.conf"
    cp "$KEYS" "$TARGET/home/user/.ssh/authorized_keys" || cannot "cannot copy authorized_keys"
    chmod 0700 "$TARGET/home/user/.ssh"; chmod 0600 "$TARGET/home/user/.ssh/authorized_keys"
    "$CHROOT" "$TARGET" chown -R 1000:1000 /home/user || cannot "chown of /home/user failed"
    "$CHROOT" "$TARGET" systemctl enable sshd NetworkManager sddm qemu-guest-agent || cannot "systemctl enable failed"
    # D9: plymouth renders the sync unit's messages. The hook goes right
    # after udev (plymouth's own documented place); the theme is set here, and
    # plan 3's screenshot is the proof that the one chosen renders text.
    [[ -f "$etc/mkinitcpio.conf" ]] || cannot "no $etc/mkinitcpio.conf after pacstrap"
    grep -q '^HOOKS=.*plymouth' "$etc/mkinitcpio.conf" || sed -i 's/^HOOKS=(\(.*\)udev /HOOKS=(\1udev plymouth /' "$etc/mkinitcpio.conf"
    grep -q '^HOOKS=.*plymouth' "$etc/mkinitcpio.conf" || cannot "could not add the plymouth hook to mkinitcpio.conf (no 'udev' in HOOKS)"
    "$CHROOT" "$TARGET" plymouth-set-default-theme cachyos || "$CHROOT" "$TARGET" plymouth-set-default-theme script || cannot "no plymouth theme that renders text"
    "$CHROOT" "$TARGET" flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo || cannot "flatpak remote-add failed"
    say "configure: $HOST_NAME, user (uid 1000, NOPASSWD, autologin), sshd key-only, plymouth, Flathub"
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
    mkdir -p "$etc/das-backup" "$etc/systemd/system" "$etc/xdg/autostart" "$TARGET/home/user/Desktop"
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
    ln -sfn /usr/share/das-backup/docs/EMERGENCY-QUICK-REFERENCE.md "$TARGET/home/user/Desktop/Quick Reference — PRINT THIS.md"
    ln -sfn /usr/share/das-backup/docs/DISASTER-RECOVERY-GUIDE.md "$TARGET/home/user/Desktop/Recovery Guide.md"
    ln -sfn /usr/share/das-backup/docs/STORAGE-ARCHITECTURE-AND-RECOVERY.md "$TARGET/home/user/Desktop/STORAGE-ARCHITECTURE-AND-RECOVERY.md"
    "$CHROOT" "$TARGET" chown -h -R 1000:1000 /home/user/Desktop || cannot "chown of the Desktop links failed"
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
    local n count=0 failed=
    mount_top
    for n in $(replaced_names); do
        # Only the names this provision set aside: an OS name (or an
        # --also-replace archive) followed by .replaced.<stamp>. A received
        # subvolume that merely carries the suffix is not one of ours.
        on_allowlist "${n%.replaced.$STAMP}" || { say "leaving $n: not a name this provision set aside"; continue; }
        "$BTRFS" property set -ts "$TOP/$n" ro false 2>/dev/null || :
        # Every name is attempted and every failure named; exit 2 at the end.
        if "$BTRFS" subvolume delete "$TOP/$n"; then
            say "deleted $n"; count=$((count + 1))
        else
            say "could not delete $n"; failed=1
        fi
    done
    say "delete-replaced: $count subvolume(s) deleted"
    [[ -z "$failed" ]] || exit 2
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
