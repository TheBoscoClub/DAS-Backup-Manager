#!/bin/bash
# recovery-os-install.sh — the guest-side installer of a DAS recovery drive's
# OS. Runs as root INSIDE the CachyOS live system, in the drive's own VM
# (recovery-os-updater-<label>), driven step by step by recovery-os-vm.sh
# provision through the guest agent. The host never writes to the drive.
# Version: 1.4.2
# Date: 2026-10-13
#
# Steps, in the order the driver runs them:
#   replace-os       p2 (by UUID) at $TOP: refuse a wrong UUID, a wrong p1
#                    PARTUUID, or a p2 with no received backup; copy p1 into the
#                    subvolume @esp.replaced.<stamp> and compare; write the
#                    manifest $TOP/.das-provision.<stamp> (one line per set-aside
#                    name); rename the OS subvolumes on the allowlist to
#                    <name>.replaced.<stamp>; create @ and @home; format p1 FAT32
#                    with the bay label; mount @, @home, p1 under $TARGET
#   pacstrap         the baseline, from the live system's own repositories
#   configure        fstab, hostname, locale, tz, keymap, user, sudoers,
#                    autologin, sshd, pacman.conf, mkinitcpio, plymouth, Flathub
#   package          the project package; marker; sync unit; masks; desktop
#   boot             bootctl install, loader.conf, the entries, mkinitcpio -P
#   verify           the loader files and every file the entries name; fstab
#                    devices; btrdasd (in the target) says btrbk at boot: no
#   unmount          everything under $TARGET, then $TOP
#   delete-replaced  (Phase 3, from the NEW OS) delete every set-aside name in
#                    every manifest, nested subvolumes first, each by name
#   undo             walk every manifest newest first: the current original
#                    becomes <name>.unbooted.<stamp>, the set-aside name comes
#                    back; then p1 is restored from the oldest @esp copy;
#                    refused once the new @ has booted
#   --count-packages print the baseline's package count (the driver's dry run)
# Exit: 0 done, 2 cannot read or a tool failed, 3 refused. Every parameter is
# a DAS_INSTALL_* variable; the list is in the plan
# (docs/superpowers/plans/2026-10-12-recovery-provision-plan.md, Task 1) plus
# DAS_INSTALL_ESP_PARTUUID (p1's PARTUUID, checked before anything is touched)
# and DAS_INSTALL_CONFIG (a config.toml for the verify step's btrdasd inspect).
#
# Deletion and undo act ONLY on names a manifest lists, never on a pattern
# match: the manifest is written atomically before the first rename.
set -euo pipefail

STEP=${1:-}
ROOT="${DAS_INSTALL_ROOT:-}" # test seam
TOP="${DAS_INSTALL_TOP:-$ROOT/mnt/top}"
TARGET="${DAS_INSTALL_TARGET:-$ROOT/mnt/target}"
ESP_MNT="${DAS_INSTALL_ESP_MNT:-$ROOT/mnt/esp-old}"
UUID="${DAS_INSTALL_UUID:-}"
ESP_LABEL="${DAS_INSTALL_ESP_LABEL:-}"
ESP_PARTUUID="${DAS_INSTALL_ESP_PARTUUID:-}"
BAY="${DAS_INSTALL_BAY:-}"
LETTER="${DAS_INSTALL_LETTER:-}"
HOST_NAME="${DAS_INSTALL_HOSTNAME:-}"
STAMP="${DAS_INSTALL_STAMP:-}"
ALSO="${DAS_INSTALL_ALSO_REPLACE:-}"
PACKAGE="${DAS_INSTALL_PACKAGE:-}"
KEYS="${DAS_INSTALL_KEYS:-}"
CONFIG="${DAS_INSTALL_CONFIG:-}"
PKG_SHA="${DAS_INSTALL_PAYLOAD_SHA256:-}"
DEV="${DAS_INSTALL_DEV:-}"
BTRFS="${DAS_INSTALL_BTRFS:-btrfs}"
MOUNT="${DAS_INSTALL_MOUNT:-mount}"
UMOUNT="${DAS_INSTALL_UMOUNT:-umount}"
MKFS_VFAT="${DAS_INSTALL_MKFS_VFAT:-mkfs.vfat}"
BLKID="${DAS_INSTALL_BLKID:-blkid}"
PACSTRAP="${DAS_INSTALL_PACSTRAP:-pacstrap}"
CHROOT="${DAS_INSTALL_ARCH_CHROOT:-arch-chroot}"
GENFSTAB="${DAS_INSTALL_GENFSTAB:-genfstab}"
CP="${DAS_INSTALL_CP:-cp}"
MV="${DAS_INSTALL_MV:-mv}"
SYNC="${DAS_INSTALL_SYNC:-sync}"

say() { printf '%s\n' "$*"; }
refuse() {
    say "$* -- refused"
    exit 3
}
cannot() {
    say "$*"
    exit 2
}
need() {
    local v
    for v in "$@"; do [[ -n "${!v}" ]] || cannot "DAS_INSTALL_$v is not set"; done
}

# ---- the allowlist -------------------------------------------------------
# A name is replaced when it is EXACTLY @, @home, @rescue, @.previous or
# @.previous.new (the writable clone the pipeline's slot step makes, and its
# staging); or @.pre-update. /
# @.half-upgraded. followed by exactly a stamp (YYYYMMDD-HHMM, optionally SS,
# the shape recovery-os-vm.sh's snapshot step writes) and nothing after; or is
# one of DAS_INSTALL_ALSO_REPLACE (each exactly @.archive.<chars>). A name
# containing .replaced. or .unbooted. is never on it: it is already ours.
# No bracket ranges (bd 1bsx): a range like 0-9 or A-Z can match more than
# ASCII in another locale; the classes are spelled out.
ASCII_ALNUM=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789
OS_SNAP_RE='^@\.(pre-update|half-upgraded)\.[[:digit:]]{8}-[[:digit:]]{4}([[:digit:]]{2})?$'
ARCHIVE_RE="^@\\.archive\\.[${ASCII_ALNUM}._-]+\$"
ALSO_LIST=()

ours_already() { [[ "$1" == *.replaced.* || "$1" == *.unbooted.* ]]; }
archive_name_ok() { [[ "$1" =~ $ARCHIVE_RE ]] && ! ours_already "$1"; }
# Every step runs this first: a bad stamp or ALSO entry refuses here, so
# delete-replaced and undo enforce the same inputs replace-os does.
validate_inputs() {
    local a
    [[ "$STAMP" =~ ^[[:digit:]]{8}-[[:digit:]]{6}$ ]] || refuse "DAS_INSTALL_STAMP '$STAMP' is not YYYYMMDD-HHMMSS"
    read -r -a ALSO_LIST <<<"$ALSO"
    for a in "${ALSO_LIST[@]}"; do
        archive_name_ok "$a" || refuse "'$a' is not an @.archive.* subvolume"
    done
}
# A name this installer may ever set aside, for validating a manifest line.
valid_orig() {
    local n=$1
    ours_already "$n" && return 1
    case "$n" in @ | @home | @rescue | @.previous | @.previous.new | @esp) return 0 ;; esac
    [[ "$n" =~ $OS_SNAP_RE ]] || archive_name_ok "$n"
}
on_allowlist() {
    local n=$1 a
    ours_already "$n" && return 1
    case "$n" in @ | @home | @rescue | @.previous | @.previous.new) return 0 ;; esac
    [[ "$n" =~ $OS_SNAP_RE ]] && return 0
    for a in "${ALSO_LIST[@]}"; do [[ "$n" != "$a" ]] || return 0; done
    return 1
}

# ---- the top level -------------------------------------------------------
NAMES_LIST=()
# Top-level subvolume names: "path <name>" lines of subvolume list -o, names
# with no slash. A failed listing is exit 2, never an empty top level.
load_names() {
    local out
    out=$("$BTRFS" subvolume list -o "$TOP") || cannot "cannot list the subvolumes of $TOP"
    mapfile -t NAMES_LIST < <(sed -n 's/.* path //p' <<<"$out" | grep -v / || :)
}
present() { [[ -e "$TOP/$1" || -L "$TOP/$1" ]]; }
# move <from> <to>, both names under $TOP: the destination is never overwritten.
move() {
    present "$1" || cannot "cannot rename $1: it is absent"
    ! present "$2" || cannot "$2 already exists: not overwritten"
    "$MV" -T -- "$TOP/$1" "$TOP/$2" || cannot "cannot rename $1 to $2"
}
flush() { "$SYNC" -f "$1" || cannot "cannot flush $1 to disk"; }
flush_fs() { "$BTRFS" filesystem sync "$TOP" || cannot "cannot flush $TOP to disk"; }
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

# ---- the manifest --------------------------------------------------------
# $TOP/.das-provision.<stamp>[.deleting|.undoing]: "<original>TAB<set-aside
# name>" per line, then "#end <count>" (a manifest cut short is refused).
# Renamed .deleting when delete-replaced begins, .undoing when undo begins,
# .done / .undone when they finished; only the first three are live.
list_manifests() { # oldest first
    local f
    for f in "$TOP"/.das-provision.*; do
        [[ -f "$f" ]] || continue
        if [[ "${f##*/}" =~ ^\.das-provision\.[[:digit:]]{8}-[[:digit:]]{6}(\.deleting|\.undoing)?$ ]]; then printf '%s\n' "$f"; fi
    done | LC_ALL=C sort
}
MAN_STAMP=""
MAN_ORIG=()
MAN_SA=()
read_manifest() {
    local line o s n=0 end=""
    MAN_STAMP=${1##*/}
    MAN_STAMP=${MAN_STAMP#.das-provision.}
    MAN_STAMP=${MAN_STAMP%.deleting}
    MAN_STAMP=${MAN_STAMP%.undoing}
    MAN_ORIG=()
    MAN_SA=()
    while IFS= read -r line || [[ -n "$line" ]]; do
        [[ -n "$line" ]] || continue
        [[ -z "$end" ]] || cannot "${1##*/}: lines after #end"
        if [[ "$line" == "#end "* ]]; then
            end=${line#"#end "}
            continue
        fi
        IFS=$'\t' read -r o s <<<"$line"
        valid_orig "$o" || cannot "${1##*/}: '$o' is not a name this installer ever sets aside (a truncated line?)"
        [[ "$s" == "$o.replaced.$MAN_STAMP" ]] || cannot "${1##*/}: '$s' is not $o.replaced.$MAN_STAMP (a truncated line?)"
        MAN_ORIG+=("$o")
        MAN_SA+=("$s")
        n=$((n + 1))
    done <"$1" || cannot "cannot read ${1##*/}"
    [[ -n "$end" ]] || cannot "${1##*/} has no #end line: it was cut short"
    [[ "$end" == "$n" ]] || cannot "${1##*/}: #end says $end entries, $n found: it was cut short"
    ((n > 0)) || cannot "${1##*/} lists nothing"
}
# A manifest's file name without its state suffix.
manifest_base() {
    local b=${1##*/}
    b=${b%.deleting}
    printf '%s' "${b%.undoing}"
}

# ---- the ESP copy --------------------------------------------------------
# A tree as sorted "<sha256>  ./path" lines, then its directories. A failing
# find, sort or sha256sum is a failure (exit 2 in the caller's $( )), never an
# empty list that would compare equal to another empty list.
tree_listing() {
    local files dirs
    files=$(cd "$1" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum) || cannot "cannot list or hash the files of $1"
    dirs=$(cd "$1" && find . -type d | LC_ALL=C sort) || cannot "cannot list or hash the directories of $1"
    printf '%s\n%s\n' "$files" "$dirs"
}
check_esp() { # p1's PARTUUID is the configured one, or exit 3
    local part
    part=$("$BLKID" -s PARTUUID -o value "${DEV}1") || part=""
    [[ -n "$part" ]] || refuse "p1 of $DEV has no readable PARTUUID: not this drive's ESP"
    [[ "${part,,}" == "${ESP_PARTUUID,,}" ]] || refuse "p1 of $DEV is PARTUUID $part, not the configured $ESP_PARTUUID: not this drive's ESP"
}
ESP_ID_RE='^[[:xdigit:]]{4}-[[:xdigit:]]{4}$'
ESP_ID=""
# format_esp <XXXX-XXXX>: FAT32 with the bay label AND the given volume ID (the
# old OS's fstab and the project's docs name p1 by it); the ID is read back.
format_esp() {
    local want=$1 got
    "$MKFS_VFAT" -F 32 -i "${want//-/}" -n "$ESP_LABEL" "${DEV}1" || cannot "cannot format p1 as FAT32 ($ESP_LABEL)"
    got=$("$BLKID" -p -s UUID -o value "${DEV}1") || got=""
    [[ "${got^^}" == "${want^^}" ]] || cannot "p1 was formatted but its volume ID is '$got', not $want: refusing to go on"
}
# matches_list <dir> <esp.list>: the tree's content is what the list recorded.
matches_list() {
    local have
    [[ -f "$2" ]] || return 1
    have=$(tree_listing "$1") || return 2
    [[ "$have" == "$(<"$2")" ]]
}
save_esp() {
    local dst="$TOP/@esp.replaced.$STAMP" want have
    ESP_ID=$("$BLKID" -p -s UUID -o value "${DEV}1") || ESP_ID=""
    [[ "$ESP_ID" =~ $ESP_ID_RE ]] || refuse "p1's volume ID '$ESP_ID' is unreadable or not of the form XXXX-XXXX: it could not be restored"
    mkdir -p "$ESP_MNT"
    "$BTRFS" subvolume create "$dst" >/dev/null || cannot "cannot create @esp.replaced.$STAMP"
    mkdir -p "$dst/esp"
    "$MOUNT" -o ro "${DEV}1" "$ESP_MNT" || cannot "cannot mount p1 read-only to copy it"
    want=$(tree_listing "$ESP_MNT") || {
        "$UMOUNT" "$ESP_MNT" || :
        cannot "cannot read p1"
    }
    "$CP" -a "$ESP_MNT/." "$dst/esp/" || {
        "$UMOUNT" "$ESP_MNT" || :
        cannot "cannot copy p1 into @esp.replaced.$STAMP"
    }
    "$UMOUNT" "$ESP_MNT" || cannot "cannot unmount p1 after copying it"
    have=$(tree_listing "$dst/esp") || cannot "cannot read the copy of p1"
    if [[ "$have" != "$want" ]]; then
        "$BTRFS" subvolume delete "$dst" >/dev/null || say "could not remove the incomplete copy @esp.replaced.$STAMP"
        refuse "the copy of p1 in @esp.replaced.$STAMP differs from p1 (nothing renamed, p1 not formatted)"
    fi
    printf '%s\n' "$want" >"$dst/esp.list" || cannot "cannot write esp.list"
    printf '%s\n' "$ESP_ID" >"$dst/esp.uuid" || cannot "cannot write esp.uuid"
    flush "$dst"
    say "p1 copied into @esp.replaced.$STAMP, compared by sha256, esp.list written, synced"
}

step_replace_os() {
    need UUID ESP_LABEL ESP_PARTUUID DEV
    local seen n todo=() mf f live=()
    seen=$("$BLKID" -s UUID -o value "${DEV}2") || seen=""
    [[ -n "$seen" ]] || refuse "p2 of $DEV has no readable UUID: not this drive, or not a filesystem"
    [[ "${seen,,}" == "${UUID,,}" ]] || refuse "p2 of $DEV is $seen, not the configured $UUID: not this drive"
    check_esp
    mount_top
    received_present || refuse "p2 holds no received backup subvolume: not a recovery drive to rebuild (seed it with a backup run first)"
    load_names
    # Undo orders manifests by stamp and takes the oldest as the original, so a
    # new stamp must be strictly newer than every manifest, in any state.
    for f in "$TOP"/.das-provision.*; do
        [[ -e "$f" ]] || continue
        if [[ "${f##*/}" =~ ^\.das-provision\.([[:digit:]]{8}-[[:digit:]]{6})(\.deleting|\.undoing|\.done|\.undone)?$ ]]; then
            [[ "$STAMP" > "${BASH_REMATCH[1]}" ]] || refuse "stamp $STAMP is not newer than ${f##*/} (stamp ${BASH_REMATCH[1]}): undo would take the wrong ESP as the original"
        fi
    done
    mapfile -t live < <(list_manifests | grep -E '\.(undoing|deleting)$' || :)
    if ((${#live[@]} > 0)); then
        for f in "${live[@]}"; do say "${f##*/}"; done
        refuse "an undo or delete-replaced did not finish (above): finish undo / delete-replaced first"
    fi
    for n in "${NAMES_LIST[@]}"; do
        on_allowlist "$n" || continue
        ! present "$n.replaced.$STAMP" || cannot "$n.replaced.$STAMP already exists: not overwritten"
        todo+=("$n")
    done
    ! present "@esp.replaced.$STAMP" || cannot "@esp.replaced.$STAMP already exists: not overwritten"
    mf="$TOP/.das-provision.$STAMP"
    for f in "$TOP"/.das-provision."$STAMP"*; do
        [[ ! -e "$f" ]] || cannot "${f##*/} already exists: a provision with this stamp already ran, the stamp is not reused"
    done
    if [[ -n "$(list_manifests)" ]]; then
        say "earlier manifests found: an incomplete provision; its set-aside names stay, the current ones are set aside under $STAMP"
    fi
    save_esp
    # The manifest first: written, synced, placed atomically, the directory
    # synced; only then the first rename.
    {
        for n in "${todo[@]}"; do printf '%s\t%s\n' "$n" "$n.replaced.$STAMP"; done
        printf '@esp\t@esp.replaced.%s\n' "$STAMP"
        printf '#end %s\n' $((${#todo[@]} + 1))
    } >"$mf.new" || cannot "cannot write the manifest"
    flush "$mf.new"
    "$MV" -T -- "$mf.new" "$mf" || cannot "cannot place the manifest"
    flush "$TOP"
    for n in "${todo[@]}"; do
        say "setting aside $n as $n.replaced.$STAMP"
        move "$n" "$n.replaced.$STAMP"
    done
    "$BTRFS" subvolume create "$TOP/@" >/dev/null || cannot "cannot create @"
    "$BTRFS" subvolume create "$TOP/@home" >/dev/null || cannot "cannot create @home"
    # Everything above must be on disk before p1 is formatted: a power-off
    # right after the format must not leave p1 empty with no copy to restore.
    flush_fs
    format_esp "$ESP_ID"
    mkdir -p "$TARGET"
    "$MOUNT" -o "subvol=/@,compress=zstd:3,noatime" "UUID=$UUID" "$TARGET" || cannot "cannot mount @ at $TARGET"
    mkdir -p "$TARGET/home" "$TARGET/boot"
    "$MOUNT" -o "subvol=/@home,compress=zstd:3,noatime" "UUID=$UUID" "$TARGET/home" || cannot "cannot mount @home"
    "$MOUNT" "${DEV}1" "$TARGET/boot" || cannot "cannot mount p1 at $TARGET/boot"
    say "replace-os: OS subvolumes set aside as *.replaced.$STAMP (manifest ${mf##*/}); @ and @home created; p1 formatted $ESP_LABEL; mounted at $TARGET"
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
    flush "$boot"
    say "boot: loader.conf and 2 entries written, images built, p1 flushed"
}

step_verify() {
    local e key val f dev missing=0 rec rc=0 n=0 req boot="$TARGET/boot"
    need LETTER CONFIG
    for e in "$boot"/loader/entries/*.conf; do [[ -f "$e" ]] && n=$((n + 1)); done
    ((n > 0)) || cannot "no loader entries on the ESP ($boot/loader/entries)"
    for req in loader/loader.conf loader/entries/cachyos-recovery.conf loader/entries/cachyos-recovery-fallback.conf EFI/systemd/systemd-bootx64.efi EFI/BOOT/BOOTX64.EFI; do
        [[ -f "$boot/$req" ]] || {
            say "$req is not on the ESP"
            missing=1
        }
    done
    for e in "$boot"/loader/entries/*.conf; do
        [[ -f "$e" ]] || continue
        while read -r key val; do
            case "$key" in
                linux | initrd | efi)
                    f="$boot/${val#/}"
                    [[ -f "$f" ]] || {
                        say "${val#/} named by $(basename "$e") is not on the ESP"
                        missing=1
                    }
                    ;;
            esac
        done <"$e"
    done
    [[ -r "$TARGET/etc/fstab" ]] || cannot "no $TARGET/etc/fstab"
    while read -r dev _; do
        case "$dev" in
            UUID=* | LABEL=* | PARTUUID=* | PARTLABEL=*)
                [[ -n "$ROOT" ]] || findfs "$dev" >/dev/null || {
                    say "/etc/fstab names $dev, which does not resolve"
                    missing=1
                }
                ;;
        esac
    done <"$TARGET/etc/fstab"
    ((missing == 0)) || refuse "verify: a file or device is missing (above)"
    # btrdasd is installed in the TARGET, not in the live system. inspect needs
    # a config.toml (a fresh target has none until its first boot derives one),
    # so the payload's is copied in for the call and removed after. Exit 1 is
    # "needs attention" (a reading was made); only 2 is "could not read".
    mkdir -p "$TARGET/tmp"
    cp "$CONFIG" "$TARGET/tmp/das-install-inspect.toml" || cannot "cannot copy $CONFIG into the target"
    rec=$("$CHROOT" "$TARGET" btrdasd recovery-os inspect --root / --label "system-recovery-$LETTER-2tb" --config /tmp/das-install-inspect.toml 2>&1) || rc=$?
    rm -f "$TARGET/tmp/das-install-inspect.toml"
    ((rc <= 1)) || cannot "btrdasd recovery-os inspect failed ($rc): $rec"
    say "$rec"
    grep -Eq '^[[:space:]]+btrbk at boot[[:space:]]+no([[:space:]]|$)' <<<"$rec" || refuse "the new root's boot record is not 'no'"
    say "verify: the loader files and every file the entries name are on the ESP; every fstab device resolves; btrbk at boot: no"
}

step_unmount() {
    "$UMOUNT" -R "$TARGET" || cannot "cannot unmount $TARGET"
    "$UMOUNT" "$TOP" || cannot "cannot unmount $TOP"
    say "unmount: done"
}

# delete_tree <set-aside name>: its nested subvolumes (systemd makes
# var/lib/machines and var/lib/portables), deepest first, each by name, then
# the name itself. A glob never reaches a delete.
delete_tree() {
    local sa=$1 all p nested=()
    all=$("$BTRFS" subvolume list "$TOP") || {
        say "could not list the subvolumes under $sa"
        return 1
    }
    mapfile -t nested < <(sed -n 's/.* path //p' <<<"$all" | sed 's|^<FS_TREE>/||' | grep -F -- "$sa/" | LC_ALL=C sort -r || :)
    for p in "${nested[@]}"; do
        [[ "$p" == "$sa/"* ]] || continue
        "$BTRFS" subvolume delete "$TOP/$p" || {
            say "could not delete $p"
            return 1
        }
        say "deleted $p"
    done
    "$BTRFS" subvolume delete "$TOP/$sa" || {
        say "could not delete $sa"
        return 1
    }
    say "deleted $sa"
}

BOOTED_REL="var/lib/das-recovery/booted" # written by das-recovery-state-sync.sh on every boot

step_delete_replaced() {
    need UUID
    local mfs=() mf i b newest prov failed="" count=0 marker="$TOP/@/$BOOTED_REL"
    mount_top
    load_names
    mapfile -t mfs < <(list_manifests)
    ((${#mfs[@]} > 0)) || cannot "no provision manifest (.das-provision.<stamp>) on this filesystem: nothing to delete"
    for mf in "${mfs[@]}"; do
        [[ "$mf" != *.undoing ]] || refuse "${mf##*/}: an undo began and did not finish; delete-replaced refused (run undo again)"
    done
    for mf in "${mfs[@]}"; do read_manifest "$mf"; done # all valid before any delete
    # The old OS is deleted only after the new one has been seen to boot: the
    # marker the sync unit wrote (provisioned=<stamp> from the recovery-drive
    # marker the package step wrote) must name the NEWEST live manifest's stamp.
    [[ -f "$marker" ]] || refuse "the new @ has not booted ($BOOTED_REL does not exist): the old OS is kept"
    b=$(manifest_base "${mfs[$((${#mfs[@]} - 1))]}")
    newest=${b#.das-provision.}
    prov=$(sed -n 's/^provisioned=//p' "$marker" | head -1) || prov=""
    if [[ -z "$prov" ]]; then
        refuse "the booted marker names no provision (a sync unit older than 1.1.2 writes no provision: the payload's package may be too old); the newest manifest is $newest, the old OS is kept"
    fi
    [[ "$prov" == "$newest" ]] || refuse "the booted marker says provisioned='$prov', the newest manifest is $newest: it does not prove this provision booted; the old OS is kept"
    for i in "${!mfs[@]}"; do
        mf=${mfs[$i]}
        if [[ "$mf" != *.deleting ]]; then
            ! present "${mf##*/}.deleting" || cannot "${mf##*/}.deleting already exists"
            "$MV" -T -- "$mf" "$mf.deleting" || cannot "cannot mark ${mf##*/} as deleting"
            mfs[i]="$mf.deleting"
        fi
    done
    for mf in "${mfs[@]}"; do
        read_manifest "$mf"
        for i in "${!MAN_SA[@]}"; do
            if ! present "${MAN_SA[$i]}"; then
                say "${MAN_SA[$i]} is already absent"
                continue
            fi
            if delete_tree "${MAN_SA[$i]}"; then count=$((count + 1)); else failed=1; fi
        done
    done
    say "delete-replaced: $count subvolume(s) deleted"
    [[ -z "$failed" ]] || cannot "delete-replaced: something could not be deleted (above); the manifests stay"
    for mf in "${mfs[@]}"; do
        b=${mf##*/}
        move "$b" "${b%.deleting}.done"
    done
}

step_undo() {
    need UUID ESP_LABEL ESP_PARTUUID DEV
    local mfs=() mf i orig sa remaining=() oldest_stamp esp_dir b rc mounted
    mount_top
    load_names
    mapfile -t mfs < <(list_manifests | LC_ALL=C sort -r)
    ((${#mfs[@]} > 0)) || refuse "no provision manifest on this filesystem: nothing to undo"
    # Preflight: nothing is changed until every manifest passes.
    check_esp
    read_manifest "${mfs[$((${#mfs[@]} - 1))]}"
    oldest_stamp=$MAN_STAMP
    for mf in "${mfs[@]}"; do
        read_manifest "$mf"
        if [[ "$mf" == *.deleting ]]; then
            for sa in "${MAN_SA[@]}"; do ! present "$sa" || remaining+=("$sa"); done
            refuse "delete-replaced began (${mf##*/}) and may have deleted part of the old OS; undo refused. Still present: ${remaining[*]:-nothing}"
        fi
        # Any root a provision installed (the current @, or the @.replaced.<s>
        # of a provision newer than the oldest) that has booted is not undone.
        if [[ "$MAN_STAMP" != "$oldest_stamp" ]] && present "@.replaced.$MAN_STAMP/$BOOTED_REL"; then
            refuse "the root installed by an earlier provision has booted (@.replaced.$MAN_STAMP/$BOOTED_REL exists): undo refused"
        fi
        for i in "${!MAN_ORIG[@]}"; do
            orig=${MAN_ORIG[$i]}
            sa=${MAN_SA[$i]}
            if present "$sa"; then
                if [[ "$orig" == "@" ]] && present "@/$BOOTED_REL"; then
                    refuse "the new @ has booted ($BOOTED_REL exists): undo refused"
                fi
                if [[ "$orig" != "@esp" ]] && present "$orig"; then
                    ! present "$orig.unbooted.$MAN_STAMP" || cannot "$orig.unbooted.$MAN_STAMP already exists: not overwritten"
                fi
            elif [[ "$orig" == "@esp" ]] || ! present "$orig"; then
                refuse "$sa is gone and $orig is absent: part of the old OS was deleted; undo refused"
            fi
        done
    done
    # The old ESP copy must be whole BEFORE anything is renamed or formatted.
    esp_dir="$TOP/@esp.replaced.$oldest_stamp"
    [[ -d "$esp_dir/esp" ]] || cannot "the old ESP copy @esp.replaced.$oldest_stamp/esp is missing"
    rc=0
    matches_list "$esp_dir/esp" "$esp_dir/esp.list" || rc=$?
    ((rc != 2)) || cannot "cannot read the old ESP copy"
    ((rc == 0)) || refuse "the old ESP copy @esp.replaced.$oldest_stamp does not match its esp.list (or the list is missing)"
    ESP_ID=""
    [[ ! -f "$esp_dir/esp.uuid" ]] || ESP_ID=$(<"$esp_dir/esp.uuid") || ESP_ID=""
    [[ "$ESP_ID" =~ $ESP_ID_RE ]] || refuse "@esp.replaced.$oldest_stamp/esp.uuid is missing or not of the form XXXX-XXXX: p1's volume ID could not be restored"
    # Mark every live manifest .undoing, synced, before the first rename: a
    # delete-replaced can no longer run, and a re-run of undo picks them up.
    for i in "${!mfs[@]}"; do
        mf=${mfs[$i]}
        if [[ "$mf" != *.undoing ]]; then
            b=${mf##*/}
            move "$b" "$b.undoing"
            mfs[i]="$mf.undoing"
        fi
    done
    flush "$TOP"
    for mf in "${mfs[@]}"; do
        read_manifest "$mf"
        for i in "${!MAN_ORIG[@]}"; do
            orig=${MAN_ORIG[$i]}
            sa=${MAN_SA[$i]}
            [[ "$orig" != "@esp" ]] || continue
            if ! present "$sa"; then
                say "$orig was not renamed by $MAN_STAMP (or is already back): left as is"
                continue
            fi
            if present "$orig"; then
                move "$orig" "$orig.unbooted.$MAN_STAMP"
                say "$orig set aside as $orig.unbooted.$MAN_STAMP"
            fi
            move "$sa" "$orig"
            say "$sa renamed back to $orig"
        done
    done
    flush_fs
    # p1 comes back from the OLDEST manifest's copy: the ESP as it was before
    # the first provision of the chain; compared by sha256 afterwards.
    mkdir -p "$ESP_MNT"
    format_esp "$ESP_ID"
    "$MOUNT" "${DEV}1" "$ESP_MNT" || cannot "cannot mount p1 to restore the old ESP"
    rc=0
    mounted=1
    "$CP" -a "$esp_dir/esp/." "$ESP_MNT/" || rc=2
    # Flush, unmount, mount again read-only, then compare: a compare on the
    # mount that was written would read the page cache, not the disk.
    if ((rc == 0)); then "$SYNC" -f "$ESP_MNT" || rc=2; fi
    if ((rc == 0)); then
        # mounted follows what each call did, so a failed remount is never
        # followed by an unmount that hides it.
        if "$UMOUNT" "$ESP_MNT"; then mounted=0; else rc=2; fi
    fi
    if ((rc == 0)); then
        if "$MOUNT" -o ro "${DEV}1" "$ESP_MNT"; then mounted=1; else rc=2; fi
    fi
    if ((rc == 0)); then matches_list "$ESP_MNT" "$esp_dir/esp.list" || rc=$?; fi
    if ((mounted == 1)); then "$UMOUNT" "$ESP_MNT" || cannot "cannot unmount p1 after restoring it"; fi
    ((rc != 2)) || cannot "cannot restore the old ESP to p1 (copy, flush, unmount, remount or read-back failed)"
    ((rc == 0)) || refuse "the restored p1 differs from @esp.replaced.$oldest_stamp/esp.list"
    for mf in "${mfs[@]}"; do
        b=${mf##*/}
        move "$b" "${b%.undoing}.undone"
    done
    flush "$TOP"
    say "undo: the old OS is back; the unbooted new roots are *.unbooted.<stamp> (delete them by hand); p1 restored from @esp.replaced.$oldest_stamp (also delete by hand)"
}

case "$STEP" in
    --count-packages) wc -w <<<"$BASELINE" ;;
    replace-os | pacstrap | configure | package | boot | verify | unmount | delete-replaced | undo)
        validate_inputs
        "step_${STEP//-/_}"
        ;;
    *)
        say "usage: recovery-os-install.sh replace-os|pacstrap|configure|package|boot|verify|unmount|delete-replaced|undo|--count-packages"
        exit 2
        ;;
esac
