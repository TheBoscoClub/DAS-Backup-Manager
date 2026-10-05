#!/bin/bash
# shellcheck disable=SC2016
# SC2016: the single-quoted text below is the source of stub programs, and the
#   literal text of a line in boot-archive-cleanup.sh; expanding it would defeat both.
#
# tests/test_boot_archive_cleanup.sh
#
# scripts/boot-archive-cleanup.sh, the pruner of the boot-subvolume archives
# (@.archive.<TS> / @home.archive.<TS>) backup-run.sh leaves on the primary
# backup target, run for real, end to end, in a sandbox:
#
#   - What it may delete (bd DAS-Backup-Manager-1bsx). Two guards stand between
#     a line of `btrfs subvolume list` and `btrfs subvolume delete`: the path
#     may hold only a short list of characters, and the name must be exactly
#     @<letters, digits, _ or ->.archive.<8 digits>T<6 digits>. Under
#     en_US.UTF-8 bash's regex ranges [A-Za-z] and [0-9] also match accented
#     and fullwidth letters and the digits of other scripts, so the guards
#     passed @<e-acute>.archive.20200101T000000 and it was deleted. The cases
#     below need such a locale: where bash's own ranges match no non-ASCII
#     letter or digit (C and C.UTF-8 show nothing) they could not fail, so
#     they print NOT RUN instead of passing. The ASCII cases run everywhere.
#
# How: the REAL script runs from a copy in which exactly one line differs, the
# root check, which reads DAS_TEST_EUID. It runs under `env -i` with a PATH of
# stubs (btrdasd, mountpoint, btrfs) and a few harmless tools: a command the
# stubs do not cover is "command not found", which fails the case. Nothing real
# can be listed, mounted or deleted: the btrfs stub only records the path it
# was asked to delete.
#
# Writes only beneath a mktemp directory. No root, no devices, no network.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/scripts/boot-archive-cleanup.sh"
WORK="$(mktemp -d)" && [[ -d "$WORK" ]] || {
    echo "HARNESS BROKEN: no temp dir"
    exit 2
}
trap 'rm -rf "${WORK:?}"' EXIT

STATE="$WORK/state"
COPY="$WORK/boot-archive-cleanup.sh"
BIN="$WORK/bin"
SYSBIN="$WORK/sysbin"
PRIMARY="$WORK/mnt/primary"
MIRROR="$WORK/mnt/mirror"

pass=0
fail=0
harness_broken() {
    echo "HARNESS BROKEN: $*"
    exit 2
}
ok() {
    echo "ok   $1"
    pass=$((pass + 1))
}
bad() {
    echo "FAIL $1"
    fail=$((fail + 1))
}
check() { # check <name> <got> <want>
    if [[ "$2" == "$3" ]]; then
        ok "$1"
    else
        bad "$1 — got '$2', want '$3'"
    fi
}

# ---------------------------------------------------------------------------
# The sandboxed copy: one line differs
# ---------------------------------------------------------------------------
ROOT_LINE='    if [[ $EUID -ne 0 ]]; then'
n="$(grep -cxF -- "$ROOT_LINE" "$SRC")"
[[ "$n" == 1 ]] || harness_broken "expected exactly one line '$ROOT_LINE' in $SRC, found $n"
sed -e 's|^    if \[\[ \$EUID -ne 0 \]\]; then$|    if [[ ${DAS_TEST_EUID:?} -ne 0 ]]; then|' "$SRC" >"$COPY"
changed="$(diff "$SRC" "$COPY" | grep -c '^>')"
[[ "$changed" == 1 ]] || harness_broken "the copy differs from $SRC in $changed lines, not 1"

# ---------------------------------------------------------------------------
# PATH: stubs, and a few harmless tools
# ---------------------------------------------------------------------------
mkdir -p "$BIN" "$SYSBIN" "$WORK/home" "$WORK/tmp"
for tool in basename cat date mktemp rm tr; do
    path="$(type -P "$tool")" || harness_broken "no $tool on this system"
    ln -s "$path" "$SYSBIN/$tool"
done
stub() { # stub <name>: the program text on stdin, with the common preamble
    {
        echo '#!/bin/bash'
        echo 'S="$DAS_TEST_STATE"'
        cat
    } >"$BIN/$1"
    chmod +x "$BIN/$1"
}
stub btrdasd <<'EOF'
case "$1 ${2:-}" in
"config dump-env") cat "$S/env" ;;
*)
    echo "btrdasd stub: unexpected: $*" >&2
    exit 99
    ;;
esac
EOF
# mountpoint -q <path>: a mount point while $S/mounted/<name> exists.
stub mountpoint <<'EOF'
[[ -e "$S/mounted/$(basename "${*: -1}")" ]] && exit 0
exit 32
EOF
# btrfs: a label per mount point, the listing the case wrote for it, and a
# delete that only records its argument.
stub btrfs <<'EOF'
case "$1 ${2:-}" in
"filesystem label") echo "label-$(basename "$3")" ;;
"subvolume list")
    f="$S/listing.$(basename "$3")"
    if [[ ! -f "$f" ]]; then
        echo "ERROR: cannot list $3 (stub)" >&2
        exit 1
    fi
    cat "$f"
    ;;
"subvolume delete")
    echo "$3" >>"$S/deleted"
    ;;
*)
    echo "btrfs stub: unexpected: $*" >&2
    exit 99
    ;;
esac
EOF

# ---------------------------------------------------------------------------
# One run's sandbox: a config, mounts, listings
# ---------------------------------------------------------------------------
# What `btrdasd config dump-env` prints for a RAID-1 primary and a mirror.
write_env() {
    cat >"$STATE/env" <<EOF
DAS_BOOT_ARCHIVE_RETENTION_DAYS=60
DAS_ALL_TARGET_MOUNTS='$PRIMARY $MIRROR'
DAS_TARGET_COUNT=2
DAS_TARGET_0_MOUNT='$PRIMARY'
DAS_TARGET_0_ROLE='primary'
DAS_TARGET_1_MOUNT='$MIRROR'
DAS_TARGET_1_ROLE='mirror'
EOF
}
# A new sandbox: both targets mounted, no listings.
fresh() {
    rm -rf "${STATE:?}"
    mkdir -p "$STATE/mounted"
    touch "$STATE/mounted/primary" "$STATE/mounted/mirror"
    write_env
}
# The primary's listing, one `btrfs subvolume list` line per path argument.
listing() { # listing <path>...
    local id=256 p
    : >"$STATE/listing.primary"
    for p in "$@"; do
        printf 'ID %d gen %d top level 5 path %s\n' "$id" "$((id + 100))" "$p" >>"$STATE/listing.primary"
        id=$((id + 1))
    done
}
# One run of the real script: its output in $STATE/out, its status in RC.
RC=""
run_pruner() { # run_pruner <locale> [args...]
    local loc="$1"
    shift
    env -i PATH="$BIN:$SYSBIN" HOME="$WORK/home" LC_ALL="$loc" TMPDIR="$WORK/tmp" \
        DAS_TEST_STATE="$STATE" DAS_TEST_EUID=0 \
        BTRDASD_BIN="$BIN/btrdasd" DAS_CONFIG="$WORK/config.toml" \
        "$BASH" "$COPY" "$@" >"$STATE/out" 2>&1
    RC=$?
    if grep -q 'command not found' "$STATE/out"; then
        bad "the run reached a command that is neither stubbed nor whitelisted: $(grep -m1 'command not found' "$STATE/out")"
    fi
}
deleted() { # the paths it asked btrfs to delete, relative to the primary, sorted
    if [[ -f "$STATE/deleted" ]]; then
        LC_ALL=C sort "$STATE/deleted" | sed "s|^$PRIMARY/||" | tr '\n' ' '
    fi
}
said() { grep -cF -- "$1" "$STATE/out"; }

# Letters and digits that are not ASCII, as bytes: no editor or locale can
# change them.
E_ACUTE=$'\xc3\xa9'
FULLWIDTH_A=$'\xef\xbc\xa1'
ARABIC_DATE=$'\xd9\xa2\xd9\xa0\xd9\xa2\xd9\xa0\xd9\xa0\xd9\xa1\xd9\xa0\xd9\xa1T\xd9\xa0\xd9\xa0\xd9\xa0\xd9\xa0\xd9\xa0\xd9\xa0'
NOT_RUN=""
# Whether this host has a locale in which bash's own range matches what it
# should not: the cases that need one cannot fail anywhere else.
locale_widens_ranges() {
    (export LC_ALL=en_US.UTF-8; [[ $E_ACUTE =~ [A-Za-z] && $ARABIC_DATE =~ ^[0-9]+T ]]) 2>/dev/null # locale-range-ok: the probe
}

# ---------------------------------------------------------------------------
echo "== what the pruner may delete: the guards name their characters (bd 1bsx)"
# ---------------------------------------------------------------------------
# Expired (2020) and well-formed: pruned, in the top directory or below a plain
# one. A future one: kept. Not an archive: ignored. And those that are not what
# the guards mean — an accented letter, a fullwidth one, a date in Arabic-Indic
# digits, and an accented directory above a well-formed name: never deleted,
# each said. The name guard cannot see a directory, so the last one is the path
# guard's alone; for the others the two guards back each other up, and the
# path guard is the stricter.
ASCII_OLD="@.archive.20200101T000000"
HOME_OLD="@home.archive.20200101T000000"
DIR_OLD="dir/@.archive.20200101T000000"
FUTURE="@.archive.20990101T000000"
OTHER="nvme/root-.20261004T0300"
ACCENT_NAME="@${E_ACUTE}.archive.20200101T000000"
FULLWIDTH_NAME="@${FULLWIDTH_A}.archive.20200101T000000"
ARABIC_NAME="@.archive.${ARABIC_DATE}"
ACCENT_DIR="dir-${E_ACUTE}/@.archive.20200101T000000"

fresh
listing "$ASCII_OLD" "$HOME_OLD" "$DIR_OLD" "$FUTURE" "$OTHER" "$ACCENT_NAME" "$FULLWIDTH_NAME" "$ARABIC_NAME" "$ACCENT_DIR"
run_pruner C
check "C locale, real run: exit status" "$RC" "0"
check "C locale: only the three expired, well-formed archives are deleted" \
    "$(deleted)" "$ASCII_OLD $HOME_OLD $DIR_OLD "
check "C locale: the accented, fullwidth, Arabic-Indic and accented-directory names are each refused, by name" \
    "$(said "Unrecognized archive path - NOT deleted: $ACCENT_NAME")$(said "Unrecognized archive path - NOT deleted: $FULLWIDTH_NAME")$(said "Unrecognized archive path - NOT deleted: $ARABIC_NAME")$(said "Unrecognized archive path - NOT deleted: $ACCENT_DIR")" "1111"
check "C locale: the future archive is kept, the rest counted" \
    "$(said 'Deleted 3, kept 1, errors 0')" "1"

if locale_widens_ranges; then
    fresh
    listing "$ASCII_OLD" "$HOME_OLD" "$DIR_OLD" "$FUTURE" "$OTHER" "$ACCENT_NAME" "$FULLWIDTH_NAME" "$ARABIC_NAME" "$ACCENT_DIR"
    run_pruner en_US.UTF-8
    check "en_US.UTF-8, real run: exit status" "$RC" "0"
    check "en_US.UTF-8: only the three expired, well-formed archives are deleted" \
        "$(deleted)" "$ASCII_OLD $HOME_OLD $DIR_OLD "
    check "en_US.UTF-8: the accented name is refused, by name" \
        "$(said "Unrecognized archive path - NOT deleted: $ACCENT_NAME")" "1"
    check "en_US.UTF-8: the fullwidth name is refused, by name" \
        "$(said "Unrecognized archive path - NOT deleted: $FULLWIDTH_NAME")" "1"
    check "en_US.UTF-8: the Arabic-Indic date is refused, by name, not read as a date that will not parse" \
        "$(said "Unrecognized archive path - NOT deleted: $ARABIC_NAME")$(said 'Could not parse timestamp')" "10"
    check "en_US.UTF-8: the well-formed name below an accented directory is refused, by the path guard" \
        "$(said "Unrecognized archive path - NOT deleted: $ACCENT_DIR")" "1"
    check "en_US.UTF-8: the future archive is kept, the rest counted" \
        "$(said 'Deleted 3, kept 1, errors 0')" "1"
else
    NOT_RUN="the en_US.UTF-8 cases: bash's [A-Za-z] and [0-9] match no non-ASCII letter or digit here (locale missing?)"
    echo "NOT RUN: $NOT_RUN"
fi

echo
echo "passed=$pass failed=$fail"
[[ $fail -eq 0 ]] || exit 1
[[ -z "$NOT_RUN" ]] || echo "NOT RUN: $NOT_RUN"
echo "BOOT ARCHIVE CLEANUP SUITE GREEN"
