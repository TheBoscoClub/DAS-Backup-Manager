#!/bin/bash
# tests/test_pkgbuild_syntax.sh -- packaging/arch/PKGBUILD parses as makepkg
# sources it, and its arrays hold what they say. A single quote closed early
# (drive\'s inside '...': a backslash escapes nothing there) made the whole
# file fail to source, unseen until makepkg ran (2026-10-13; since 9119407).
set -uo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKGBUILD="$REPO/packaging/arch/PKGBUILD"
passes=0 fails=0
check() {
    if [[ "$2" == "$3" ]]; then
        printf 'ok   %s\n' "$1"
        passes=$((passes + 1))
    else
        printf 'FAIL %s: got %s, want %s\n' "$1" "$2" "$3"
        fails=$((fails + 1))
    fi
}

# What makepkg does first: source it in bash.
out=$(bash -n "$PKGBUILD" 2>&1) && rc=0 || rc=$?
check "PKGBUILD parses (bash -n)" "$rc${out:+: $out}" "0"

# Sourced in a clean shell: the arrays and functions are what they claim.
read_pkgbuild() {
    # shellcheck disable=SC2016 # the inner shell expands them, after sourcing the PKGBUILD
    env -i bash -c 'source "$1" && printf "%s\n" "pkgname=$pkgname" "optdepends=${#optdepends[@]}" "build=$(type -t build)" "package=$(type -t package)" && printf "opt:%s\n" "${optdepends[@]}"' _ "$1" 2>&1
}
src=$(read_pkgbuild "$PKGBUILD") && rc=0 || rc=$?
check "PKGBUILD sources" "$rc" "0"
check "pkgname" "$(sed -n 's/^pkgname=//p' <<<"$src")" "das-backup-manager"
check "build() is a function" "$(sed -n 's/^build=//p' <<<"$src")" "function"
check "package() is a function" "$(sed -n 's/^package=//p' <<<"$src")" "function"
# Each optdepends entry is one "name: why" -- a quote closed early splits one
# entry into several or swallows the next.
bad=$(grep '^opt:' <<<"$src" | grep -vcE '^opt:[[:lower:][:digit:]._+-]+: ' || :)
check "every optdepends entry is 'name: why'" "$bad" "0"
check "the tigervnc entry is whole" "$(grep -c "^opt:tigervnc: the GUI opens a recovery drive's console with vncviewer on the helper's socket\$" <<<"$src" || :)" "1"

# The check itself can fail: the defect, put back into a copy.
tmp=$(mktemp)
work=$(mktemp -d)
trap 'rm -rf "${tmp:?}" "${work:?}"' EXIT
sed "s|\"tigervnc: the GUI opens a recovery drive's console with vncviewer on the helper's socket\"|'tigervnc: the GUI opens a recovery drive\\\\'s console with vncviewer on the helper\\\\'s socket'|" "$PKGBUILD" >"$tmp"
check "the counter-case is the old defect" "$(grep -c "drive\\\\'s console" "$tmp" || :)" "1"
bash -n "$tmp" >/dev/null 2>&1 && rc=0 || rc=$?
check "the old defect fails to parse" "$([[ $rc -ne 0 ]] && echo refused || echo parsed)" "refused"

# Removal hooks (bd DAS-Backup-Manager-hzxr): each format's removal runs
# `btrdasd setup --uninstall --force`, only on removal, and never fails it.
# The copy under test has the manifest path pointed at a file of ours, and
# btrdasd is a recording stub on a clean PATH.
INSTALL_HOOK="$REPO/packaging/arch/das-backup-manager.install"
PRERM="$REPO/packaging/debian/prerm"
SPEC="$REPO/packaging/fedora/das-backup-manager.spec"
bash -n "$INSTALL_HOOK" && rc=0 || rc=$?
check "arch hook parses" "$rc" "0"
sh -n "$PRERM" && rc=0 || rc=$?
check "debian prerm parses (sh -n)" "$rc" "0"
# shellcheck disable=SC2016 # the inner shell expands it
check "PKGBUILD names the install hook" "$(env -i bash -c 'source "$1" && echo "$install"' _ "$PKGBUILD")" "das-backup-manager.install"
check "debian prerm is executable" "$([[ -x "$PRERM" ]] && echo yes || echo no)" "yes"

mkdir "$work/bin"
touch "$work/manifest"
cat >"$work/bin/btrdasd" <<STUB
#!/bin/bash
echo "\$*" >>"$work/calls"
exit "\${STUB_RC:-0}"
STUB
chmod 755 "$work/bin/btrdasd"
sed "s|/etc/das-backup/.manifest|$work/manifest|g" "$INSTALL_HOOK" >"$work/arch.sh"
sed "s|/etc/das-backup/.manifest|$work/manifest|g" "$PRERM" >"$work/prerm.sh"
# The spec's scriptlet: the lines between %preun and %files
sed -n '/^%preun/,/^%files/p' "$SPEC" | sed '1d;$d' | sed "s|/etc/das-backup/.manifest|$work/manifest|g" >"$work/preun.sh"
runhook() { # <stub exit> <command...>: the hook's exit and what the stub saw
    : >"$work/calls"
    env -i PATH="$work/bin:/usr/bin:/bin" STUB_RC="$1" "${@:2}" >/dev/null 2>&1 && rc=0 || rc=$?
    echo "rc=$rc calls=$(tr '\n' '|' <"$work/calls")"
}
want="rc=0 calls=setup --uninstall --force|"
none="rc=0 calls="
check "arch pre_remove runs the uninstall" "$(runhook 0 bash -c "source '$work/arch.sh'; pre_remove")" "$want"
check "arch pre_remove survives a failing uninstall" "$(runhook 75 bash -c "source '$work/arch.sh'; pre_remove")" "$want"
check "debian prerm remove runs the uninstall" "$(runhook 0 sh "$work/prerm.sh" remove)" "$want"
check "debian prerm survives a failing uninstall" "$(runhook 75 sh "$work/prerm.sh" remove)" "$want"
check "debian prerm upgrade leaves units alone" "$(runhook 0 sh "$work/prerm.sh" upgrade)" "$none"
check "rpm preun 0 (removal) runs the uninstall" "$(runhook 0 sh "$work/preun.sh" 0)" "$want"
check "rpm preun 1 (upgrade) leaves units alone" "$(runhook 0 sh "$work/preun.sh" 1)" "$none"
check "rpm preun survives a failing uninstall" "$(runhook 75 sh "$work/preun.sh" 0)" "$want"
rm "${work:?}/manifest"
check "no manifest, no uninstall" "$(runhook 0 sh "$work/prerm.sh" remove)" "$none"

echo
echo "$passes passed, $fails failed"
((fails == 0)) || exit 1
echo "PKGBUILD SYNTAX SUITE GREEN"
