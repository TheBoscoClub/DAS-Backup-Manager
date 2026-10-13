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
  "subvolume list") [[ -z "${STUB_LIST_FAILS:-}" ]] || exit 1
      if [[ "$3" == -o ]]; then ls -1 "$FIX_TOP"; else (cd "$FIX_TOP" && find . -name .subvol -printf '%h\n' | sed 's|^\./||'); fi \
          | sed 's/^/ID 256 gen 1 top level 5 path /' ;;
  "subvolume show") f=$3; [[ -f "$f/.subvol" ]] || exit 1
      if grep -q received "$f/.subvol"; then printf '\tReceived UUID: \t1234-5678\n'; else printf '\tReceived UUID: \t-\n'; fi
      if grep -q ro "$f/.subvol"; then printf '\tFlags: \treadonly\n'; else printf '\tFlags: \t-\n'; fi ;;
  "subvolume create") mkdir -p "$3" && echo rw >"$3/.subvol" ;;
  "subvolume delete") [[ -z "${STUB_DELETE_FAILS:-}" ]] || exit 1
      [[ -z "${STUB_DELETE_FAIL_ONLY:-}" || "$3" != *"$STUB_DELETE_FAIL_ONLY" ]] || exit 1
      rm -rf "$3" ;;
  "filesystem sync") : ;;
  *) exit 2 ;;
esac
EOF
for t in mount umount mkfs.vfat pacstrap arch-chroot genfstab; do
  # shellcheck disable=SC2016 # the stub's own script, written as is
  printf '#!/bin/bash\necho "%s $*" >>"$STUB_LOG"\n[[ -z "${STUB_%s_FAILS:-}" ]] || exit 1\n' "$t" "${t//[.-]/_}" >"$BIN/$t"
done
# STUB_mount_RO_FAILS: only a read-only mount fails (undo's remount of p1).
# shellcheck disable=SC2016 # the stub's own script, written as is
printf '[[ -z "${STUB_mount_RO_FAILS:-}" || " $* " != *" -o ro "* ]] || exit 1\n' >>"$BIN/mount"
cat >"$BIN/mkfs.vfat" <<'EOF'
#!/bin/bash
echo "mkfs.vfat $*" >>"$STUB_LOG"
[[ -z "${STUB_mkfs_vfat_FAILS:-}" ]] || exit 1
rm -rf "${FIX_ESP:?}"/* # formatting empties p1
# the new filesystem's volume ID: the -i given, else a random one
id=FFFF0000
while (($#)); do [[ "$1" == -i ]] && id=$2; shift; done
[[ -z "${STUB_MKFS_ID_WRONG:-}" ]] || id=AAAABBBB
printf '%s-%s\n' "${id:0:4}" "${id:4:4}" >"$STUB_P1UUID"
EOF
cat >"$BIN/arch-chroot" <<'EOF'
#!/bin/bash
echo "arch-chroot $*" >>"$STUB_LOG"
[[ -z "${STUB_arch_chroot_FAILS:-}" ]] || exit 1
if [[ "$2" == btrdasd ]]; then cat "$STUB_INSPECT"; exit "${STUB_INSPECT_RC:-0}"; fi
EOF
cat >"$BIN/blkid" <<'EOF'
#!/bin/bash
echo "blkid $*" >>"$STUB_LOG"
case "$*" in *PARTUUID*) cat "$STUB_PARTUUID" ;; *sdz1*) cat "$STUB_P1UUID" ;; *) cat "$STUB_BLKID" ;; esac
EOF
cat >"$BIN/cpwrap" <<'EOF'
#!/bin/bash
echo "cp $*" >>"$STUB_LOG"
/usr/bin/cp "$@" || exit 1
if [[ -n "${STUB_CP_SHORT:-}" ]]; then find "${@: -1}" -type f | head -1 | xargs -r rm -f; fi
EOF
cat >"$BIN/sync" <<'EOF'
#!/bin/bash
echo "sync $*" >>"$STUB_LOG"
[[ -z "${STUB_SYNC_FAIL:-}" || "$*" != *"$STUB_SYNC_FAIL"* ]] || exit 1
EOF
cat >"$BIN/find" <<'EOF'
#!/bin/bash
[[ -z "${STUB_FIND_FAILS:-}" ]] || exit 1
n=$(($(cat "$STUB_FINDCOUNT" 2>/dev/null || echo 0) + 1)); echo "$n" >"$STUB_FINDCOUNT"
[[ -z "${STUB_FIND_FAIL_NTH:-}" || "$n" -ne "$STUB_FIND_FAIL_NTH" ]] || exit 1
exec /usr/bin/find "$@"
EOF
cat >"$BIN/mvwrap" <<'EOF'
#!/bin/bash
# Logs every mv; STUB_MV_FAIL_AT=<n> fails the n-th call; with STUB_REQUIRE_MANIFEST
# an OS subvolume may be set aside only once the stamp's manifest exists and names it.
n=$(($(cat "$STUB_MVCOUNT" 2>/dev/null || echo 0) + 1)); echo "$n" >"$STUB_MVCOUNT"
from=${*: -2:1}; to=${*: -1}
echo "mv $from $to" >>"$STUB_LOG"
if [[ -n "${STUB_MV_FAIL_AT:-}" && "$n" -eq "$STUB_MV_FAIL_AT" ]]; then echo "mv: stubbed failure" >&2; exit 1; fi
if [[ -n "${STUB_REQUIRE_MANIFEST:-}" && "$to" == *".replaced.$DAS_INSTALL_STAMP" ]]; then
    grep -qF "$(basename "$from")" "$FIX_TOP/.das-provision.$DAS_INSTALL_STAMP" 2>/dev/null || { echo "mv: the manifest does not list it yet" >&2; exit 1; }
fi
exec /usr/bin/mv "$@"
EOF
chmod +x "$BIN"/*

fixture() {
    rm -rf "$R"; : >"$LOG"
    mkdir -p "$R/mnt/top" "$R/mnt/target" "$R/payload" "$R/etc"
    for s in @ @home @rescue @.pre-update.20261011-1016 @.half-upgraded.20261011-1017; do
        mkdir -p "$R/mnt/top/$s"; echo rw >"$R/mnt/top/$s/.subvol"; echo "old-$s" >"$R/mnt/top/$s/sentinel"
    done
    mkdir -p "$R/mnt/esp-old"; echo "old esp" >"$R/mnt/esp-old/old-esp-file"
    printf 'aaaaaaaa-01\n' >"$WORK/partuuid.out"
    printf 'ABCD-0003\n' >"$WORK/p1uuid.out"
    printf '[general]\n' >"$R/payload/config.toml"
    for s in nvme/root-.20261012T0348 nvme/das-state.20261012T0348 hdd-system/ISOs.20261012T0348; do
        mkdir -p "$R/mnt/top/$s"; echo "ro received" >"$R/mnt/top/$s/.subvol"
    done
    printf '5eed0008-0000-4000-8000-000000000008\n' >"$WORK/blkid.out"
    printf 'RECOVERY OS\n  system-recovery-A-2tb  (/)\n    btrbk config        /etc/btrbk/btrbk.conf (1 byte)\n    btrbk at boot       no — btrbk.timer masked\n' >"$WORK/inspect.out"
    printf 'pkg\n' >"$R/payload/das-backup-manager-0.7.23.0-1-x86_64.pkg.tar.zst"
    printf 'ssh-ed25519 AAAA test\n' >"$R/payload/authorized_keys"
    printf '[options]\nHoldPkg = pacman\n[cachyos]\nInclude = x\n[core]\nInclude = y\n' >"$R/etc/pacman.conf"
}
run() { # <step> [VAR=value ...]
    local step=$1; shift
    RC=0; : >"$WORK/findcount"
    OUT=$(env PATH="$BIN:$PATH" STUB_LOG="$LOG" FIX_TOP="$R/mnt/top" STUB_BLKID="$WORK/blkid.out" STUB_INSPECT="$WORK/inspect.out" STUB_PARTUUID="$WORK/partuuid.out" STUB_P1UUID="$WORK/p1uuid.out" STUB_FINDCOUNT="$WORK/findcount" \
        FIX_ESP="$R/mnt/esp-old" DAS_INSTALL_ESP_MNT="$R/mnt/esp-old" DAS_INSTALL_ESP_PARTUUID=aaaaaaaa-01 \
        DAS_INSTALL_CONFIG="$R/payload/config.toml" DAS_INSTALL_CP="$BIN/cpwrap" \
        DAS_INSTALL_SYNC="$BIN/sync" DAS_INSTALL_MV="$BIN/mvwrap" STUB_MVCOUNT="$WORK/mvcount" STUB_REQUIRE_MANIFEST=1 \
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
for s in @ @home @rescue @.pre-update.20261011-1016 @.half-upgraded.20261011-1017; do
    check "replace-os: $s renamed to $s.replaced.20261013-010203" test -d "$R/mnt/top/$s.replaced.20261013-010203"
done
check "replace-os: a new @ exists" test -f "$R/mnt/top/@/.subvol"
check "replace-os: a new @home exists" test -f "$R/mnt/top/@home/.subvol"
for s in nvme/root-.20261012T0348 nvme/das-state.20261012T0348 hdd-system/ISOs.20261012T0348; do
    check "replace-os: received $s untouched" grep -q received "$R/mnt/top/$s/.subvol"
done
check "replace-os: nothing deleted" test "$(calls 'subvolume delete')" -eq 0
check "replace-os: p1 formatted with the bay label" has "$LOG" "mkfs.vfat -F 32 -i ABCD0003 -n RECOV-ESP-1 /dev/sdz1"
check "replace-os: @ mounted at the target" has "$LOG" "mount -o subvol=/@"
check "replace-os: p1 mounted at target/boot" has "$LOG" "mount /dev/sdz1 $R/mnt/target/boot"

echo "== replace-os: the allowlist is exact (review focus 1)"
fixture
EXACT_LIST=(@home-old @.pre-update-notes @rescue2 root-.20261012T0348 old@.pre-update.20261001 @x.half-upgraded.20261001
    @.pre-update.20261011-1016x @.pre-update.20261011-1016-bak @.pre-update.2026 @.replaced.20260101-000000
    @.pre-update.20261011-1016.replaced.20260101-000000 @home.unbooted.20260101-000000)
for s in "${EXACT_LIST[@]}"; do mkdir -p "$R/mnt/top/$s"; echo "ro received" >"$R/mnt/top/$s/.subvol"; done
run replace-os
check "exact: exit 0" test "$RC" -eq 0
for s in "${EXACT_LIST[@]}"; do
    check "exact: $s is still there, unrenamed" test -f "$R/mnt/top/$s/.subvol"
done

echo "== replace-os: @.previous (the writable clone the slot step makes) by exact name"
fixture
mkdir -p "$R/mnt/top/@.previous"; echo rw >"$R/mnt/top/@.previous/.subvol"
PREV_LOOKALIKES=(@.previous-old @.previousx @.previous.20261001 x@.previous)
for s in "${PREV_LOOKALIKES[@]}"; do mkdir -p "$R/mnt/top/$s"; echo "ro received" >"$R/mnt/top/$s/.subvol"; done
run replace-os
check "previous: exit 0" test "$RC" -eq 0
check "previous: @.previous set aside" test -d "$R/mnt/top/@.previous.replaced.20261013-010203"
check "previous: @.previous listed in the manifest" grep -q "^@.previous	@.previous.replaced.20261013-010203$" "$R/mnt/top/.das-provision.20261013-010203"
for s in "${PREV_LOOKALIKES[@]}"; do
    check "previous: lookalike $s untouched" test -f "$R/mnt/top/$s/.subvol"
done

fixture
mkdir -p "$R/mnt/top/@.previous"; echo rw >"$R/mnt/top/@.previous/.subvol"; echo "the clone" >"$R/mnt/top/@.previous/sentinel"
run replace-os
run undo
check "previous, undo: exit 0" test "$RC" -eq 0
check "previous, undo: @.previous is back, itself" grep -qx "the clone" "$R/mnt/top/@.previous/sentinel"

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
check "same stamp twice: exit 3 (not strictly newer), never overwrites a set-aside name" test "$RC" -eq 3
check "same stamp twice: named" out_has "not newer than .das-provision.20261013-010203"

S1=20261013-010203
S2=20261013-020000
TAB=$'\t'
TOPD="$R/mnt/top"
chain() { # S1 leaves a partial @ and a partial p1, then S2 runs over it
    fixture
    run replace-os
    : >"$TOPD/@/partial1"
    rm -f "$R/mnt/esp-old"/*
    echo p >"$R/mnt/esp-old/partial-esp"
    run replace-os DAS_INSTALL_STAMP=$S2
}

booted_new() { # the new OS has booted: its marker names the provision (the newest live manifest's stamp)
    local st
    st=$(find "$TOPD" -mindepth 1 -maxdepth 1 -printf '%f\n' | sed -n 's/^\.das-provision\.\([0-9]\{8\}-[0-9]\{6\}\)\(\.deleting\|\.undoing\)\{0,1\}$/\1/p' | sort | tail -1)
    mkdir -p "$TOPD/@/var/lib/das-recovery"
    printf 'provisioned=%s\n' "${1-$st}" >"$TOPD/@/var/lib/das-recovery/booted"
}
ln1() { grep -n -m1 -E -- "$1" "$LOG" | cut -d: -f1 || :; }
after() { awk -v n="$1" -v p="$2" 'NR>n && $0 ~ p {print NR; exit}' "$LOG"; }

echo "== replace-os: p1 is checked by PARTUUID, copied and compared before anything is touched (B)"
fixture; run replace-os
check "esp: p1's PARTUUID is read" has "$LOG" "blkid -s PARTUUID -o value /dev/sdz1"
check "esp: the old ESP is in @esp.replaced.<stamp>" test -f "$TOPD/@esp.replaced.$S1/esp/old-esp-file"
check "esp: p1 mounted read-only for the copy" has "$LOG" "mount -o ro /dev/sdz1"
check "esp: copied before formatted" test "$(grep -n 'mount -o ro /dev/sdz1' "$LOG" | head -1 | cut -d: -f1)" -lt "$(grep -n 'mkfs.vfat' "$LOG" | head -1 | cut -d: -f1)"
fixture; printf 'ffffffff-02\n' >"$WORK/partuuid.out"; run replace-os
check "esp partuuid mismatch: exit 3" test "$RC" -eq 3
check "esp partuuid mismatch: names both" out_has "p1 of /dev/sdz is PARTUUID ffffffff-02, not the configured aaaaaaaa-01"
check "esp partuuid mismatch: nothing renamed" test -f "$TOPD/@/.subvol" -a ! -d "$TOPD/@.replaced.$S1"
check "esp partuuid mismatch: no mkfs" test "$(calls 'mkfs.vfat')" -eq 0
check "esp partuuid mismatch: no copy, no manifest" test ! -e "$TOPD/@esp.replaced.$S1" -a ! -e "$TOPD/.das-provision.$S1"
fixture; : >"$WORK/partuuid.out"; run replace-os
check "unreadable partuuid: exit 3, never a match" test "$RC" -eq 3
check "unreadable partuuid: no mkfs" test "$(calls 'mkfs.vfat')" -eq 0
fixture; run replace-os STUB_CP_SHORT=1
check "esp copy differs: exit 3" test "$RC" -eq 3
check "esp copy differs: said" out_has "differs from p1 (nothing renamed, p1 not formatted)"
check "esp copy differs: no mkfs" test "$(calls 'mkfs.vfat')" -eq 0
check "esp copy differs: nothing renamed, no manifest" test -f "$TOPD/@/.subvol" -a ! -d "$TOPD/@.replaced.$S1" -a ! -e "$TOPD/.das-provision.$S1"
fixture; run replace-os STUB_mount_FAILS=1
check "esp cannot be mounted: exit 2" test "$RC" -eq 2
check "esp cannot be mounted: nothing renamed, no mkfs" test -f "$TOPD/@/.subvol" -a "$(calls 'mkfs.vfat')" -eq 0

echo "== replace-os: the manifest, written before the first rename (A)"
fixture; run replace-os
MF="$TOPD/.das-provision.$S1"
check "manifest: written" test -f "$MF"
for s in @ @home @rescue @.pre-update.20261011-1016 @.half-upgraded.20261011-1017; do
    check "manifest: $s" grep -qxF "$s$TAB$s.replaced.$S1" "$MF"
done
check "manifest: the ESP copy" grep -qxF "@esp${TAB}@esp.replaced.$S1" "$MF"
check "manifest: six entries and the #end count, nothing else" test "$(wc -l <"$MF")" -eq 7 -a "$(tail -1 "$MF")" = "#end 6"
check "manifest: no .new left behind" test ! -e "$MF.new"
fixture; : >"$TOPD/.das-provision.$S1"; run replace-os
check "manifest of this stamp exists: exit 3 (not strictly newer)" test "$RC" -eq 3
check "manifest of this stamp exists: nothing renamed" test -f "$TOPD/@/.subvol" -a ! -d "$TOPD/@.replaced.$S1"
fixture; mkdir -p "$TOPD/@esp.replaced.$S1"; run replace-os
check "@esp.replaced.<stamp> exists: exit 2, never overwritten" test "$RC" -eq 2

echo "== every step validates the stamp and ALSO (C)"
fixture; run replace-os
mkdir -p "$TOPD/nvme.replaced.$S1"; echo "ro received" >"$TOPD/nvme.replaced.$S1/.subvol"; : >"$LOG"
for st in replace-os delete-replaced undo pacstrap configure package boot verify unmount; do
    run "$st" DAS_INSTALL_STAMP=2026
    check "$st, bad stamp: exit 3" test "$RC" -eq 3
    run "$st" DAS_INSTALL_ALSO_REPLACE=nvme
    check "$st, ALSO=nvme: exit 3" test "$RC" -eq 3
    check "$st, ALSO=nvme: refused by name" out_has "'nvme' is not an @.archive.* subvolume -- refused"
    run "$st" DAS_INSTALL_ALSO_REPLACE="@.archive.20260601T0300.replaced.x"
    check "$st, an ALSO name that is already ours: exit 3" test "$RC" -eq 3
done
check "bad inputs: nothing deleted, look-alike untouched" test "$(calls 'subvolume delete')" -eq 0 -a -f "$TOPD/nvme.replaced.$S1/.subvol"

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

echo "== verify: the loader files, every file the entries name, every fstab device, btrbk at boot: no (G)"
vfix() { # a complete target tree
    fixture
    local b="$R/mnt/target/boot" e
    mkdir -p "$b/loader/entries" "$b/EFI/systemd" "$b/EFI/BOOT" "$R/mnt/target/etc"
    printf 'default cachyos-recovery.conf\n' >"$b/loader/loader.conf"
    for e in cachyos-recovery cachyos-recovery-fallback; do
        printf 'linux /vmlinuz-linux-cachyos\ninitrd /amd-ucode.img\ninitrd /initramfs-linux-cachyos.img\n' >"$b/loader/entries/$e.conf"
    done
    : >"$b/vmlinuz-linux-cachyos"; : >"$b/amd-ucode.img"; : >"$b/initramfs-linux-cachyos.img"; : >"$b/EFI/systemd/systemd-bootx64.efi"; : >"$b/EFI/BOOT/BOOTX64.EFI"
    printf 'UUID=5eed0008-0000-4000-8000-000000000008 / btrfs subvol=/@ 0 0\n' >"$R/mnt/target/etc/fstab"
}
vfix; run verify
check "verify: exit 0" test "$RC" -eq 0
check "verify: btrdasd runs in the target's chroot, root /, a copied config" has "$LOG" "arch-chroot $R/mnt/target btrdasd recovery-os inspect --root / --label system-recovery-A-2tb --config /tmp/das-install-inspect.toml"
check "verify: the config copy is removed again" test ! -e "$R/mnt/target/tmp/das-install-inspect.toml"
check "verify: the record is quoted" out_has "btrbk at boot       no"
vfix; rm "$R/mnt/target/boot/initramfs-linux-cachyos.img"; run verify
check "verify, a missing initramfs: exit 3" test "$RC" -eq 3
check "verify, a missing initramfs: named" out_has "initramfs-linux-cachyos.img named by cachyos-recovery.conf is not on the ESP"
vfix; printf '    btrbk at boot       may\n' >"$WORK/inspect.out"; run verify
check "verify, btrbk may run: exit 3" test "$RC" -eq 3
check "verify, btrbk may run: the record is quoted" out_has "btrbk at boot       may"
vfix; rm -f "$R/mnt/target/boot/loader/entries"/*.conf; run verify
check "verify, no loader entries at all: exit 2" test "$RC" -eq 2
check "verify, no loader entries at all: said" out_has "no loader entries"
vfix; rm "$R/mnt/target/boot/EFI/systemd/systemd-bootx64.efi"; run verify
check "verify, no systemd-bootx64.efi: exit 3" test "$RC" -eq 3
check "verify, no systemd-bootx64.efi: named" out_has "EFI/systemd/systemd-bootx64.efi is not on the ESP"
vfix; rm "$R/mnt/target/boot/EFI/BOOT/BOOTX64.EFI"; run verify
check "verify, no EFI/BOOT/BOOTX64.EFI: exit 3" test "$RC" -eq 3
check "verify, no EFI/BOOT/BOOTX64.EFI: named" out_has "EFI/BOOT/BOOTX64.EFI is not on the ESP"
vfix; rm "$R/mnt/target/boot/loader/loader.conf"; run verify
check "verify, no loader.conf: exit 3" test "$RC" -eq 3
vfix; rm "$R/mnt/target/boot/loader/entries/cachyos-recovery-fallback.conf"; run verify
check "verify, no fallback entry: exit 3" test "$RC" -eq 3
vfix; run verify STUB_INSPECT_RC=2
check "verify, inspect itself failed (exit 2): exit 2, not a pass" test "$RC" -eq 2
vfix; run verify STUB_INSPECT_RC=1
check "verify, inspect exit 1 (needs attention) with 'no': still a pass" test "$RC" -eq 0

echo "== unmount"
fixture; run unmount
check "unmount: exit 0" test "$RC" -eq 0
check "unmount: the target recursively, then the top" test "$(grep 'umount' "$LOG" | tr '\n' ' ')" = "umount -R $R/mnt/target umount $R/mnt/top "

echo "== delete-replaced: the manifests' names, nested first, each by name (A, D)"
chain
check "chain: S2 exit 0" test "$RC" -eq 0
check "chain: both manifests exist" test -f "$TOPD/.das-provision.$S1" -a -f "$TOPD/.das-provision.$S2"
check "chain: S2 listed the partial @ it set aside" grep -qxF "@${TAB}@.replaced.$S2" "$TOPD/.das-provision.$S2"
check "chain: S2 did not list names S1 already set aside" test "$(wc -l <"$TOPD/.das-provision.$S2")" -eq 4
check "chain: the old root is still at @.replaced.<S1>" test "$(cat "$TOPD/@.replaced.$S1/sentinel")" = "old-@"
mkdir -p "$TOPD/@.replaced.$S1/var/lib/machines" "$TOPD/@.replaced.$S1/var/lib/portables"
echo rw >"$TOPD/@.replaced.$S1/var/lib/machines/.subvol"; echo rw >"$TOPD/@.replaced.$S1/var/lib/portables/.subvol"
mkdir -p "$TOPD/nvme.replaced.$S1"; echo "ro received" >"$TOPD/nvme.replaced.$S1/.subvol"
: >"$LOG"; booted_new; run delete-replaced
check "delete-replaced: exit 0" test "$RC" -eq 0
for s in $S1 $S2; do
    for n in @ @home @esp; do
        check "delete-replaced: $n.replaced.$s deleted by name" has "$LOG" "subvolume delete $TOPD/$n.replaced.$s"
    done
done
for n in @rescue @.pre-update.20261011-1016 @.half-upgraded.20261011-1017; do
    check "delete-replaced: $n.replaced.$S1 deleted by name" has "$LOG" "subvolume delete $TOPD/$n.replaced.$S1"
done
check "delete-replaced: nine set-aside names and two nested, nothing else" test "$(calls 'subvolume delete')" -eq 11
dl() { grep -nxF "btrfs subvolume delete $1" "$LOG" | head -1 | cut -d: -f1; }
check "delete-replaced: var/lib/machines before its parent" test "$(dl "$TOPD/@.replaced.$S1/var/lib/machines")" -lt "$(dl "$TOPD/@.replaced.$S1")"
check "delete-replaced: var/lib/portables before its parent" test "$(dl "$TOPD/@.replaced.$S1/var/lib/portables")" -lt "$(dl "$TOPD/@.replaced.$S1")"
check "delete-replaced: a received subvolume that merely looks set-aside survives (no glob)" test -f "$TOPD/nvme.replaced.$S1/.subvol"
check "delete-replaced: received untouched" grep -q received "$TOPD/nvme/root-.20261012T0348/.subvol"
check "delete-replaced: manifests renamed .done" test -f "$TOPD/.das-provision.$S1.done" -a -f "$TOPD/.das-provision.$S2.done" -a ! -e "$TOPD/.das-provision.$S1"
run delete-replaced
check "delete-replaced, no live manifest left: exit 2" test "$RC" -eq 2
fixture; run delete-replaced
check "delete-replaced, no manifest at all: exit 2, deletes nothing" test "$RC" -eq 2 -a "$(calls 'subvolume delete')" -eq 0
check "delete-replaced, no manifest: said" out_has "no provision manifest"
fixture; run replace-os; : >"$LOG"; booted_new; run delete-replaced STUB_DELETE_FAILS=1
check "delete-replaced, every delete fails: exit 2" test "$RC" -eq 2
check "delete-replaced, every delete fails: all six attempted (five OS names and the ESP copy)" test "$(calls 'subvolume delete')" -eq 6
check "delete-replaced, a delete fails: named" out_has "could not delete @.replaced.$S1"
check "delete-replaced, a delete fails: not reported done, manifest kept" test -f "$TOPD/.das-provision.$S1.deleting" -a ! -e "$TOPD/.das-provision.$S1.done"
fixture; run replace-os; : >"$LOG"; booted_new; run delete-replaced STUB_LIST_FAILS=1
check "delete-replaced, listing fails: exit 2, never '0 deleted'" test "$RC" -eq 2 -a "$(calls 'subvolume delete')" -eq 0
check "delete-replaced, listing fails: said" out_has "cannot list the subvolumes"
check "delete-replaced, listing fails: manifest untouched" test -f "$TOPD/.das-provision.$S1"

echo "== undo: the chain, newest first; refusals (D)"
chain; run undo
check "undo: exit 0" test "$RC" -eq 0
for s in @ @home @rescue @.pre-update.20261011-1016 @.half-upgraded.20261011-1017; do
    check "undo: $s is the OLD one (sentinel)" test "$(cat "$TOPD/$s/sentinel")" = "old-$s"
done
check "undo: S1's partial @ is under @.unbooted.<S1>" test -f "$TOPD/@.unbooted.$S1/partial1"
check "undo: S2's @ is under @.unbooted.<S2>" test -f "$TOPD/@.unbooted.$S2/.subvol"
check "undo: the partial never sits at @" test ! -e "$TOPD/@/partial1"
check "undo: no *.replaced.<stamp> left for an OS name" test ! -d "$TOPD/@.replaced.$S1" -a ! -d "$TOPD/@.replaced.$S2" -a ! -d "$TOPD/@home.replaced.$S2"
check "undo: p1 formatted again with the bay label" has "$LOG" "mkfs.vfat -F 32 -i ABCD0003 -n RECOV-ESP-1 /dev/sdz1"
check "undo: the OLDEST ESP copy is back on p1" test -f "$R/mnt/esp-old/old-esp-file"
check "undo: the partial ESP is gone" test ! -e "$R/mnt/esp-old/partial-esp"
check "undo: manifests renamed .undone" test -f "$TOPD/.das-provision.$S1.undone" -a -f "$TOPD/.das-provision.$S2.undone" -a ! -e "$TOPD/.das-provision.$S1"
check "undo: deletes nothing" test "$(calls 'subvolume delete')" -eq 0
fixture; run replace-os; run undo
check "undo (one provision): exit 0" test "$RC" -eq 0
check "undo (one provision): the old @" test "$(cat "$TOPD/@/sentinel")" = "old-@"
check "undo (one provision): the unbooted new @ set aside" test -d "$TOPD/@.unbooted.$S1"
check "undo (one provision): the old ESP back" test -f "$R/mnt/esp-old/old-esp-file"
fixture; run replace-os; mkdir -p "$TOPD/@/var/lib/das-recovery"; : >"$TOPD/@/var/lib/das-recovery/booted"; run undo
check "undo after a new boot: exit 3" test "$RC" -eq 3
check "undo after a new boot: refused" out_has "the new @ has booted (var/lib/das-recovery/booted exists): undo refused"
check "undo after a new boot: nothing renamed" test -d "$TOPD/@.replaced.$S1" -a -f "$TOPD/.das-provision.$S1"
check "undo after a new boot: p1 untouched" test "$(calls 'mkfs.vfat')" -eq 1
fixture; run undo
check "undo with nothing to undo: exit 3" test "$RC" -eq 3
check "undo with nothing to undo: says so" out_has "nothing to undo"
fixture; run replace-os; : >"$LOG"; run undo STUB_LIST_FAILS=1
check "undo, listing fails: exit 2, never 'nothing to undo'" test "$RC" -eq 2
check "undo, listing fails: nothing renamed" test -d "$TOPD/@.replaced.$S1"
check "undo, listing fails: said" out_has "cannot list the subvolumes"
fixture; run replace-os; booted_new; run delete-replaced STUB_DELETE_FAIL_ONLY="@home.replaced.$S1"; : >"$LOG"
check "partial delete: exit 2" test "$RC" -eq 2
run undo
check "partial delete then undo: exit 3" test "$RC" -eq 3
check "partial delete then undo: says a delete began" out_has "delete-replaced began"
check "partial delete then undo: the remaining names are listed" out_has "Still present: @home.replaced.$S1"
check "partial delete then undo: nothing renamed" test -d "$TOPD/@home.replaced.$S1" -a ! -d "$TOPD/@.unbooted.$S1" -a "$(calls 'mkfs.vfat')" -eq 0
fixture; run replace-os; mv "$TOPD/@rescue.replaced.$S1" "$TOPD/@rescue"; run undo
check "undo, a rename that never happened (original present, set-aside absent): exit 0" test "$RC" -eq 0
check "undo, a rename that never happened: said, left alone" out_has "@rescue was not renamed by $S1"
check "undo, a rename that never happened: @rescue still the old one" test "$(cat "$TOPD/@rescue/sentinel")" = "old-@rescue"
fixture; run replace-os; rm -rf "${TOPD:?}/@rescue.replaced.$S1"; run undo
check "undo, set-aside gone and original absent: exit 3" test "$RC" -eq 3
check "undo, set-aside gone and original absent: named" out_has "@rescue.replaced.$S1 is gone"
check "undo, set-aside gone and original absent: nothing renamed" test -d "$TOPD/@.replaced.$S1" -a ! -d "$TOPD/@.unbooted.$S1"
fixture; run replace-os; : >"$WORK/partuuid.out"; run undo
check "undo, p1 PARTUUID unreadable: exit 3" test "$RC" -eq 3
check "undo, p1 PARTUUID unreadable: nothing renamed (checked in the preflight)" test -d "$TOPD/@.replaced.$S1" -a ! -d "$TOPD/@.unbooted.$S1"

echo "== durability: the copy, the manifest and the renames are on disk before p1 is formatted (C1, I2)"
fixture; run replace-os
check "durability: replace-os exit 0 (the mv stub refuses any set-aside the manifest does not list yet)" test "$RC" -eq 0
L_copy=$(ln1 "^sync -f $TOPD/@esp.replaced.$S1\$")
L_man=$(ln1 "^sync -f $TOPD/.das-provision.$S1.new\$")
L_dir=$(after "$L_man" "^sync -f $TOPD\$")
L_mv=$(ln1 "^mv .*\\.replaced\\.$S1\$")
L_fs=$(ln1 "^btrfs filesystem sync $TOPD\$")
L_mkfs=$(ln1 "^mkfs.vfat")
check "durability: the ESP copy synced, then the manifest synced" test "$L_copy" -lt "$L_man"
check "durability: manifest placed and the top level synced before the first rename" test "$L_man" -lt "$L_dir" -a "$L_dir" -lt "$L_mv"
check "durability: filesystem sync after the renames, before mkfs" test "$L_mv" -lt "$L_fs" -a "$L_fs" -lt "$L_mkfs"
fixture; run replace-os; : >"$LOG"; run undo
U_ing=$(ln1 '^mv .*\.undoing$')
U_s1=$(after "$U_ing" "^sync -f $TOPD\$")
U_mv=$(ln1 '^mv .*\.unbooted\.')
U_last=$(grep -n '^mv ' "$LOG" | grep -v '\.undone$' | tail -1 | cut -d: -f1 || :)
U_fs=$(ln1 "^btrfs filesystem sync $TOPD\$")
U_mkfs=$(ln1 '^mkfs.vfat')
U_done=$(ln1 '^mv .*\.undone$')
U_end=$(after "$U_done" "^sync -f $TOPD\$")
check "durability, undo: manifests marked .undoing and synced before the first rename" test "$U_ing" -lt "$U_s1" -a "$U_s1" -lt "$U_mv"
check "durability, undo: filesystem sync after the last rename, before mkfs" test "$U_last" -lt "$U_fs" -a "$U_fs" -lt "$U_mkfs"
check "durability, undo: synced again after the .undone renames" test "$U_mkfs" -lt "$U_done" -a "$U_done" -lt "$U_end"

echo "== the ESP copy is checked by content, not size (C4, I3)"
fixture; run replace-os
E="$TOPD/@esp.replaced.$S1"
check "esp.list: written beside the copy" test -f "$E/esp.list"
check "esp.list: holds the sha256 of the file" grep -q "$(sha256sum "$E/esp/old-esp-file" | cut -d' ' -f1)  ./old-esp-file" "$E/esp.list"
check "esp.list: synced" has "$LOG" "sync -f $E"
rm "$E/esp/old-esp-file"; : >"$LOG"; run undo
check "undo, a file gone from the copy: exit 3" test "$RC" -eq 3
check "undo, a file gone from the copy: refused before any rename or mkfs" test -d "$TOPD/@.replaced.$S1" -a -f "$TOPD/.das-provision.$S1" -a "$(calls 'mkfs.vfat')" -eq 0 -a "$(calls 'mv ')" -eq 0
fixture; run replace-os; printf 'OLD esp\n' >"$E/esp/old-esp-file"; : >"$LOG"; run undo
check "undo, a file altered to the same size: exit 3" test "$RC" -eq 3
check "undo, same size altered: nothing renamed, p1 not formatted" test -d "$TOPD/@.replaced.$S1" -a "$(calls 'mkfs.vfat')" -eq 0 -a "$(calls 'mv ')" -eq 0
fixture; run replace-os; rm -f "$E/esp.list"; : >"$LOG"; run undo
check "undo, no esp.list: exit 3, nothing changed" test "$RC" -eq 3 -a -d "$TOPD/@.replaced.$S1" -a "$(calls 'mkfs.vfat')" -eq 0
fixture; run replace-os; : >"$LOG"; run undo STUB_CP_SHORT=1
check "undo, the restored p1 differs from esp.list: exit 3" test "$RC" -eq 3
check "undo, the restored p1 differs: said" out_has "differs from"
check "undo, the restored p1 differs: the manifests are not marked undone" test ! -e "$TOPD/.das-provision.$S1.undone"
fixture; run replace-os STUB_FIND_FAILS=1
check "a failing find is a failure, not an empty list: exit 2" test "$RC" -eq 2
check "a failing find: p1 not formatted, nothing renamed" test "$(calls 'mkfs.vfat')" -eq 0 -a -f "$TOPD/@/.subvol" -a ! -d "$TOPD/@.replaced.$S1"

echo "== a truncated manifest is refused (I1)"
fixture; run replace-os; MF="$TOPD/.das-provision.$S1"; sed -i '$d' "$MF"; : >"$LOG"; run undo
check "undo, the #end line cut off: exit 2" test "$RC" -eq 2
check "undo, #end cut off: nothing renamed" test -d "$TOPD/@.replaced.$S1" -a "$(calls 'mkfs.vfat')" -eq 0 -a "$(calls 'mv ')" -eq 0
booted_new; run delete-replaced
check "delete-replaced, the #end line cut off: exit 2, nothing deleted" test "$RC" -eq 2 -a "$(calls 'subvolume delete')" -eq 0
fixture; run replace-os; MF="$TOPD/.das-provision.$S1"; truncate -s -9 "$MF"; : >"$LOG"; run undo
check "undo, cut in the middle of an entry: exit 2, nothing renamed" test "$RC" -eq 2 -a -d "$TOPD/@.replaced.$S1" -a "$(calls 'mv ')" -eq 0
fixture; run replace-os; MF="$TOPD/.das-provision.$S1"; sed -i 's/^#end 6$/#end 5/' "$MF"; : >"$LOG"; run undo
check "undo, #end count differs: exit 2, nothing renamed" test "$RC" -eq 2 -a -d "$TOPD/@.replaced.$S1" -a "$(calls 'mv ')" -eq 0
booted_new; run delete-replaced
check "delete-replaced, #end count differs: exit 2" test "$RC" -eq 2 -a "$(calls 'subvolume delete')" -eq 0

echo "== undo refuses when ANY installed root has booted (C2)"
chain; mkdir -p "$TOPD/@.replaced.$S2/var/lib/das-recovery"; : >"$TOPD/@.replaced.$S2/var/lib/das-recovery/booted"; : >"$LOG"; run undo
check "chain, S1's root booted then S2 set it aside: exit 3" test "$RC" -eq 3
check "chain: names where the marker was found" out_has "@.replaced.$S2/var/lib/das-recovery/booted"
check "chain: nothing renamed, p1 untouched" test -d "$TOPD/@.replaced.$S1" -a -d "$TOPD/@.replaced.$S2" -a "$(calls 'mkfs.vfat')" -eq 0 -a "$(calls 'mv ')" -eq 0
chain; mkdir -p "$TOPD/@.replaced.$S1/var/lib/das-recovery"; : >"$TOPD/@.replaced.$S1/var/lib/das-recovery/booted"; run undo
check "a marker in the OLDEST set-aside (the old OS, which never was installed by us) does not refuse" test "$RC" -eq 0

echo "== delete-replaced waits for proof that the new OS booted, and not for an interrupted undo (C3)"
fixture; run replace-os; : >"$LOG"; run delete-replaced
check "delete-replaced, no booted marker: exit 3" test "$RC" -eq 3
check "delete-replaced, no booted marker: said" out_has "has not booted"
check "delete-replaced, no booted marker: nothing deleted, manifest live" test "$(calls 'subvolume delete')" -eq 0 -a -f "$TOPD/.das-provision.$S1"
fixture; run replace-os; booted_new 20250101-000000; : >"$LOG"; run delete-replaced
check "delete-replaced, a marker of ANOTHER provision: exit 3, nothing deleted" test "$RC" -eq 3 -a "$(calls 'subvolume delete')" -eq 0
check "delete-replaced, another provision's marker: the marker's stamp printed" out_has "20250101-000000"
check "delete-replaced, another provision's marker: the manifest's stamp printed" out_has "$S1"
fixture; run replace-os; booted_new ""; : >"$LOG"; run delete-replaced
check "delete-replaced, a marker with no provisioned value: exit 3, nothing deleted" test "$RC" -eq 3 -a "$(calls 'subvolume delete')" -eq 0
check "delete-replaced, no provisioned value: the old-sync-unit hint" out_has "a sync unit older than 1.1.2 writes no provision: the payload's package may be too old"
fixture; run replace-os; mkdir -p "$TOPD/@/var/lib/das-recovery"; : >"$TOPD/@/var/lib/das-recovery/booted"; run delete-replaced
check "delete-replaced, an empty marker file (the old format): exit 3" test "$RC" -eq 3
fixture; chain; booted_new "$S1"; run delete-replaced
check "delete-replaced, a chain whose marker names only the OLDER provision: exit 3" test "$RC" -eq 3 -a "$(calls 'subvolume delete')" -eq 0
fixture; run replace-os; mv "$TOPD/.das-provision.$S1" "$TOPD/.das-provision.$S1.undoing"; booted_new; : >"$LOG"; run delete-replaced
check "delete-replaced, an .undoing manifest: exit 3" test "$RC" -eq 3
check "delete-replaced, an .undoing manifest: said" out_has "undo began"
check "delete-replaced, an .undoing manifest: nothing deleted" test "$(calls 'subvolume delete')" -eq 0
fixture; run replace-os; : >"$LOG"; : >"$WORK/mvcount"; run undo STUB_MV_FAIL_AT=4
check "interrupted undo (the fourth mv fails): exit 2" test "$RC" -eq 2
check "interrupted undo: the manifest is .undoing" test -f "$TOPD/.das-provision.$S1.undoing" -a ! -e "$TOPD/.das-provision.$S1"
booted_new; : >"$LOG"; run delete-replaced
check "interrupted undo, then delete-replaced: exit 3, deletes nothing" test "$RC" -eq 3 -a "$(calls 'subvolume delete')" -eq 0
rm -f "$TOPD/@/var/lib/das-recovery/booted"; : >"$WORK/mvcount"; run undo
check "interrupted undo, undo run again: exit 0" test "$RC" -eq 0
for s in @ @home @rescue @.pre-update.20261011-1016 @.half-upgraded.20261011-1017; do
    check "interrupted undo, run again: $s is the OLD one" test "$(cat "$TOPD/$s/sentinel")" = "old-$s"
done
check "interrupted undo, run again: manifest .undone" test -f "$TOPD/.das-provision.$S1.undone" -a ! -e "$TOPD/.das-provision.$S1.undoing"

echo "== the ESP keeps its volume ID (round 4, 1)"
fixture; run replace-os
E="$TOPD/@esp.replaced.$S1"
check "esp.uuid: recorded beside the copy" test "$(cat "$E/esp.uuid")" = "ABCD-0003"
check "mkfs carries the old volume ID" has "$LOG" "mkfs.vfat -F 32 -i ABCD0003 -n RECOV-ESP-1 /dev/sdz1"
check "p1's volume ID is read with the low-level probe (blkid -p, no cache), at save and at read-back" test "$(calls 'blkid -p -s UUID -o value /dev/sdz1')" -ge 2
fixture; printf 'XYZ\n' >"$WORK/p1uuid.out"; run replace-os
check "p1 volume ID garbled: exit 3" test "$RC" -eq 3
check "p1 volume ID garbled: nothing renamed, p1 not formatted" test "$(calls 'mkfs.vfat')" -eq 0 -a -f "$TOPD/@/.subvol" -a ! -d "$TOPD/@.replaced.$S1"
fixture; : >"$WORK/p1uuid.out"; run replace-os
check "p1 volume ID unreadable: exit 3, never a pass" test "$RC" -eq 3 -a "$(calls 'mkfs.vfat')" -eq 0
fixture; run replace-os STUB_MKFS_ID_WRONG=1
check "the formatted p1 has a different volume ID: exit 2" test "$RC" -eq 2
check "the formatted p1 has a different volume ID: said" out_has "volume ID"
fixture; run replace-os; printf 'DEAD-BEEF\n' >"$WORK/p1uuid.out"; run replace-os DAS_INSTALL_STAMP=$S2
check "chain: the second copy records the partial ESP's ID" test "$(cat "$TOPD/@esp.replaced.$S2/esp.uuid")" = "DEAD-BEEF"
: >"$LOG"; run undo
check "undo formats p1 with the OLDEST copy's volume ID" has "$LOG" "mkfs.vfat -F 32 -i ABCD0003 -n RECOV-ESP-1"
check "undo: the ID read back from p1 is the old one" test "$(cat "$WORK/p1uuid.out")" = "ABCD-0003"
fixture; run replace-os; echo garbage >"$E/esp.uuid"; : >"$LOG"; run undo
check "undo, garbled esp.uuid: exit 3" test "$RC" -eq 3
check "undo, garbled esp.uuid: nothing renamed, p1 not formatted" test -d "$TOPD/@.replaced.$S1" -a "$(calls 'mkfs.vfat')" -eq 0 -a "$(calls 'mv ')" -eq 0
fixture; run replace-os; rm -f "$E/esp.uuid"; : >"$LOG"; run undo
check "undo, no esp.uuid: exit 3, nothing changed" test "$RC" -eq 3 -a -d "$TOPD/@.replaced.$S1" -a "$(calls 'mkfs.vfat')" -eq 0 -a "$(calls 'mv ')" -eq 0
fixture; run replace-os; : >"$LOG"; run undo STUB_MKFS_ID_WRONG=1
check "undo, the restored p1 has another volume ID: exit 2" test "$RC" -eq 2
check "undo, another volume ID: said" out_has "volume ID"

echo "== replace-os waits for an unfinished undo or delete (round 4, 2) and never reuses a stamp (6)"
fixture; run replace-os; : >"$WORK/mvcount"; run undo STUB_MV_FAIL_AT=4; : >"$LOG"; run replace-os DAS_INSTALL_STAMP=$S2
check "replace-os after an interrupted undo: exit 3" test "$RC" -eq 3
check "replace-os after an interrupted undo: names the manifest" out_has ".das-provision.$S1.undoing"
check "replace-os after an interrupted undo: says what to finish" out_has "finish undo"
check "replace-os after an interrupted undo: nothing created, p1 not formatted" test ! -e "$TOPD/@esp.replaced.$S2" -a ! -e "$TOPD/.das-provision.$S2" -a "$(calls 'mkfs.vfat')" -eq 0
fixture; run replace-os; booted_new; run delete-replaced STUB_DELETE_FAIL_ONLY="@home.replaced.$S1"; : >"$LOG"; run replace-os DAS_INSTALL_STAMP=$S2
check "replace-os after a partial delete: exit 3" test "$RC" -eq 3
check "replace-os after a partial delete: names the manifest" out_has ".das-provision.$S1.deleting"
check "replace-os after a partial delete: nothing renamed" test ! -e "$TOPD/@esp.replaced.$S2" -a "$(calls 'mkfs.vfat')" -eq 0
for suf in .done .undone; do
    fixture; : >"$TOPD/.das-provision.$S1$suf"; run replace-os
    check "replace-os, a $suf manifest of this stamp exists: exit 3, stamp not reused" test "$RC" -eq 3 -a -f "$TOPD/@/.subvol" -a ! -d "$TOPD/@.replaced.$S1"
done

echo "== the restored ESP is flushed before it is compared; the new one before verify (round 4, 3)"
fixture; run replace-os; : >"$LOG"; run undo
C_CP=$(ln1 '^cp -a .*/esp-old/$')
C_SY=$(ln1 "^sync -f $R/mnt/esp-old\$")
C_UM1=$(ln1 "^umount $R/mnt/esp-old\$")
C_RO=$(ln1 "^mount -o ro /dev/sdz1 $R/mnt/esp-old\$")
C_UM2=$(after "$C_RO" "^umount $R/mnt/esp-old\$")
check "undo: copied, flushed, unmounted, mounted again read-only (the compare reads the disk), unmounted" test "$C_CP" -lt "$C_SY" -a "$C_SY" -lt "$C_UM1" -a "$C_UM1" -lt "$C_RO" -a "$C_RO" -lt "$C_UM2"
fixture; run replace-os; run undo STUB_SYNC_FAIL="mnt/esp-old"
check "undo, the flush of p1 fails: exit 2" test "$RC" -eq 2
check "undo, the flush fails: manifests not marked undone" test ! -e "$TOPD/.das-provision.$S1.undone"
fixture; run replace-os; : >"$LOG"; run undo STUB_mount_RO_FAILS=1
check "undo, the read-only remount fails: exit 2" test "$RC" -eq 2
check "undo, the read-only remount fails: says so, not a misleading unmount error" out_has "remount"
check "undo, the read-only remount fails: no umount of a p1 that is not mounted" test -z "$(after "$(ln1 "^mount -o ro /dev/sdz1 $R/mnt/esp-old\$")" "^umount $R/mnt/esp-old\$")"
fixture; run boot
check "boot: p1 flushed after the images are built" test "$(ln1 'mkinitcpio -P')" -lt "$(ln1 "^sync -f $R/mnt/target/boot\$")"

echo "== every find has a guard (round 4, 4)"
for n in 1 2 3 4; do
    fixture; run replace-os STUB_FIND_FAIL_NTH=$n
    check "replace-os, find call $n fails: exit 2" test "$RC" -eq 2
    check "replace-os, find call $n fails: p1 not formatted, nothing renamed" test "$(calls 'mkfs.vfat')" -eq 0 -a -f "$TOPD/@/.subvol" -a ! -d "$TOPD/@.replaced.$S1"
done
for n in 1 2; do
    fixture; run replace-os; : >"$LOG"; run undo STUB_FIND_FAIL_NTH=$n
    check "undo preflight, find call $n fails: exit 2" test "$RC" -eq 2
    check "undo preflight, find call $n fails: nothing renamed, p1 not formatted" test -d "$TOPD/@.replaced.$S1" -a "$(calls 'mkfs.vfat')" -eq 0 -a "$(calls 'mv ')" -eq 0
done
for n in 3 4; do
    fixture; run replace-os; : >"$LOG"; run undo STUB_FIND_FAIL_NTH=$n
    check "undo restore, find call $n fails: exit 2" test "$RC" -eq 2
    check "undo restore, find call $n fails: manifests not marked undone" test ! -e "$TOPD/.das-provision.$S1.undone"
done

echo "== the package step binds the marker to this provision (round 4, 5)"
fixture; mkdir -p "$R/mnt/target/usr/lib/das-backup"; printf '[Unit]\nDescription=x\n' >"$R/mnt/target/usr/lib/das-backup/das-recovery-state-sync.service"; run package
check "package: the marker carries provisioned=<stamp>" has "$R/mnt/target/etc/das-backup/recovery-drive" "provisioned=$S1"


echo "== a stamp must be strictly newer than every manifest, in any state (round 5)"
fixture; run replace-os DAS_INSTALL_STAMP=$S2; : >"$LOG"; run replace-os
check "replace-os, a LOWER stamp than an existing manifest: exit 3" test "$RC" -eq 3
check "lower stamp: the new stamp named" out_has "$S1"
check "lower stamp: the existing stamp named" out_has "$S2"
check "lower stamp: nothing renamed, copied or formatted" test "$(calls 'mkfs.vfat')" -eq 0 -a "$(calls 'mv ')" -eq 0 -a ! -e "$TOPD/@esp.replaced.$S1" -a ! -e "$TOPD/.das-provision.$S1"
for suf in "" .deleting .undoing .done .undone; do
    fixture; : >"$TOPD/.das-provision.$S2$suf"; run replace-os
    check "replace-os, a lower stamp than a '$suf' manifest: exit 3, 'not newer'" test "$RC" -eq 3
    check "replace-os, lower than a '$suf' manifest: said" out_has "not newer than .das-provision.$S2$suf"
    check "replace-os, lower than a '$suf' manifest: nothing renamed" test -f "$TOPD/@/.subvol" -a ! -d "$TOPD/@.replaced.$S1" -a "$(calls 'mkfs.vfat')" -eq 0
done
fixture; run replace-os; run replace-os DAS_INSTALL_STAMP=$S2
check "a strictly newer stamp is accepted" test "$RC" -eq 0

echo
if ((FAILS > 0)); then printf '%d check(s) FAILED\n' "$FAILS"; exit 1; fi
echo "RECOVERY-OS-INSTALL SUITE GREEN"
