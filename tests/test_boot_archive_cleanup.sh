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
#   - What it prints, and how backup-run.sh reads it back (bd
#     DAS-Backup-Manager-zwr). run_archive_cleanup() calls the pruner and
#     refuses to call a run OK unless the pruner printed a per-target summary
#     of its own mode: "Deleted N, kept N, errors N" in a real run, "Would
#     delete N, kept N, errors N" in a dry run. The pruner's dry run printed
#     "Would keep N, found expired archives above" — which the reader never
#     matched, so every dry run of the backup recorded this step as FAILED and
#     exited 3 — while the exit suite's stub pruner printed the real run's
#     line whatever it was asked, so nothing ever saw it. Here the two are run
#     as the host runs them, the REAL reader (extracted from backup-run.sh)
#     over the REAL pruner's own output, both ways: a dry run and a real run
#     with a summary are OK; a run with none, a pruner that failed, and one
#     that printed the other mode's summary are not.
#
#   - A target it cannot tell is mounted (bd DAS-Backup-Manager-2dmn). The pruner
#     asked `mountpoint -q ... 2>/dev/null` and read every failure of it as "not
#     mounted - nothing examined": a skip, exit 0, with nothing said. It asks
#     the probe the unmount gate asks now (util-linux's exit status: 0 a mount
#     point, 32 not one, anything else could not tell; a path that is not there
#     is the absent drive's removed mount point, and not mounted): could not
#     tell is a target NOT pruned, counted, said with the probe's own message,
#     and the run exits 1, which run_archive_cleanup records as a FAIL. The two
#     functions that read the probe are one text in the two scripts, held so here.
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
PRIMARY2="$WORK/mnt/primary2"
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
# mountpoint <path>, as util-linux 2.42.4 answers (measured): 0 a mount point,
# 32 not one, 1 an error — and 1, "No such file or directory", for a path that
# is not there. A mount point while $S/mounted/<name> exists; $S/probe/<name>
# makes it answer otherwise: "error", "absent", or "rc:<n>" (that exit status,
# nothing said).
stub mountpoint <<'EOF'
path="${*: -1}"
name="$(basename "$path")"
mode="$(cat "$S/probe/$name" 2>/dev/null)"
case "$mode" in
error)
    echo "mountpoint: $path: Input/output error" >&2
    exit 1
    ;;
absent)
    echo "mountpoint: $path: No such file or directory" >&2
    exit 1
    ;;
rc:*) exit "${mode#rc:}" ;;
esac
[[ -e "$S/mounted/$name" ]] && exit 0
exit 32
EOF
# btrfs: a label per mount point, the listing the case wrote for it, and a
# delete that only records its argument.
stub btrfs <<'EOF'
echo "$*" >>"$S/btrfs.calls"
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
# The same with a second primary between them: three targets.
write_env2() {
    cat >"$STATE/env" <<EOF
DAS_BOOT_ARCHIVE_RETENTION_DAYS=60
DAS_ALL_TARGET_MOUNTS='$PRIMARY $PRIMARY2 $MIRROR'
DAS_TARGET_COUNT=3
DAS_TARGET_0_MOUNT='$PRIMARY'
DAS_TARGET_0_ROLE='primary'
DAS_TARGET_1_MOUNT='$PRIMARY2'
DAS_TARGET_1_ROLE='primary'
DAS_TARGET_2_MOUNT='$MIRROR'
DAS_TARGET_2_ROLE='mirror'
EOF
}
# A new sandbox: both targets mounted, no listings.
fresh() {
    rm -rf "${STATE:?}"
    mkdir -p "$STATE/mounted" "$STATE/probe"
    touch "$STATE/mounted/primary" "$STATE/mounted/mirror"
    write_env
}
# A new sandbox of three targets, the two primaries and the mirror mounted.
fresh2() {
    rm -rf "${STATE:?}"
    mkdir -p "$STATE/mounted" "$STATE/probe"
    touch "$STATE/mounted/primary" "$STATE/mounted/primary2" "$STATE/mounted/mirror"
    write_env2
}
# What mountpoint answers for a target (see its stub): error, absent, rc:<n>.
probe_mode() { # probe_mode <target name> <mode>
    printf '%s\n' "$2" >"$STATE/probe/$1"
}
# A target's listing, one `btrfs subvolume list` line per path argument.
listing_for() { # listing_for <target name> <path>...
    local name="$1" id=256 p
    shift
    : >"$STATE/listing.$name"
    for p in "$@"; do
        printf 'ID %d gen %d top level 5 path %s\n' "$id" "$((id + 100))" "$p" >>"$STATE/listing.$name"
        id=$((id + 1))
    done
}
listing() { listing_for primary "$@"; } # the primary's
# How often btrfs was asked to label or list a target.
btrfs_asked() { # btrfs_asked <target name>
    if [[ -f "$STATE/btrfs.calls" ]]; then
        grep -cE "^(filesystem label|subvolume list) .*/$1\$" "$STATE/btrfs.calls" || true
    else
        echo 0
    fi
}
# One run of the real script: its output in $STATE/out, its status in RC.
RC=""
run_pruner() { # run_pruner <locale> [args...]
    local loc="$1"
    shift
    rm -f "$STATE/deleted" "$STATE/btrfs.calls" # what a run does is that run's own
    env -i PATH="${RUN_PATH:-$BIN:$SYSBIN}" HOME="$WORK/home" LC_ALL="$loc" TMPDIR="$WORK/tmp" \
        DAS_TEST_STATE="$STATE" DAS_TEST_EUID=0 \
        BTRDASD_BIN="$BIN/btrdasd" DAS_CONFIG="$WORK/config.toml" \
        "$BASH" "$COPY" "$@" >"$STATE/out" 2>&1
    RC=$?
    # A command that is neither stubbed nor whitelisted fails the case — unless
    # the case took it away on purpose (ALLOW_NOT_FOUND names it).
    local unexpected
    unexpected="$(grep 'command not found' "$STATE/out" | grep -vF -- "${ALLOW_NOT_FOUND:-no such text}" || true)"
    if [[ -n "$unexpected" ]]; then
        bad "the run reached a command that is neither stubbed nor whitelisted: ${unexpected%%$'\n'*}"
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

# ---------------------------------------------------------------------------
echo "== what the pruner prints, and the reader of it: run_archive_cleanup (bd zwr)"
# ---------------------------------------------------------------------------
# The reader, as the script has it, and the pruner as the host calls it: by
# path, with its own arguments. PRUNER_FORCE=dry or real makes the pruner run in
# that mode whatever it was asked, to hand the reader the other mode's output.
extract() { sed -n "/^$1() {/,/^}/p" "$ROOT/scripts/backup-run.sh"; }
for fn in run_archive_cleanup record_op; do
    body="$(extract "$fn")"
    [[ -n "$body" ]] || harness_broken "$fn not found in backup-run.sh"
    eval "$body"
done
log_info() { echo "INFO: $*" >>"$WORK/reader.log"; }
log_warn() { echo "WARN: $*" >>"$WORK/reader.log"; }
declare -A OP_STATUS=()
cat >"$WORK/pruner" <<EOF
#!/bin/bash
case "\${PRUNER_FORCE:-}" in
dry) set -- --dryrun ;;
real) set -- ;;
esac
exec env -i PATH="$BIN:$SYSBIN" HOME="$WORK/home" LC_ALL=C TMPDIR="$WORK/tmp" \\
    DAS_TEST_STATE="$STATE" DAS_TEST_EUID=0 \\
    BTRDASD_BIN="$BIN/btrdasd" DAS_CONFIG="$WORK/config.toml" \\
    "$BASH" "$COPY" "\$@"
EOF
chmod +x "$WORK/pruner"

# read_back <dryrun|real>: the reader's verdict on the real pruner, under the
# shell options backup-run.sh runs under: "<status>|<detail>".
read_back() {
    rm -f "$STATE/deleted" # what a run deletes is that run's own
    (
        set -euo pipefail
        : >"$WORK/reader.log"
        OP_STATUS=()
        # shellcheck disable=SC2034  # read by the extracted run_archive_cleanup
        BOOT_ARCHIVE_CLEANUP_BIN="$WORK/pruner"
        run_archive_cleanup "$1"
        printf '%s|%s\n' "${OP_STATUS[archive_cleanup]:-unset}" "${OP_STATUS[archive_cleanup_detail]:-}"
    )
}
status_of() { echo "${1%%|*}"; }
reader_said() { grep -cF -- "$1" "$WORK/reader.log"; }

# Two expired archives and one in the future: a run deletes two and keeps one.
fresh
listing "$ASCII_OLD" "$HOME_OLD" "$FUTURE" "$OTHER"

run_pruner C --dryrun
check "pruner, dry run: exit status" "$RC" "0"
check "pruner, dry run: the summary has the real run's shape, counting what it would delete" \
    "$(said 'Would delete 2, kept 1, errors 0')" "1"
check "pruner, dry run: the line that no reader matched is gone" "$(said 'Would keep')" "0"
check "pruner, dry run: nothing is asked of btrfs to delete" "$(deleted)" ""
run_pruner C
check "pruner, real run: exit status" "$RC" "0"
check "pruner, real run: the summary" "$(said 'Deleted 2, kept 1, errors 0')" "1"
check "pruner, real run: the two expired archives were asked to be deleted" "$(deleted)" "$ASCII_OLD $HOME_OLD "

verdict="$(read_back dryrun)"
check "reader, a dry run over the real pruner: OK" "$(status_of "$verdict")" "OK"
check "reader, a dry run: the detail is exactly the dry run's own summary, nothing after it" \
    "$verdict" "OK|Would delete 2, kept 1, errors 0"
check "reader, a dry run: the pruner deleted nothing" "$(deleted)" ""
verdict="$(read_back real)"
check "reader, a real run over the real pruner: OK" "$(status_of "$verdict")" "OK"
check "reader, a real run: the detail is exactly the run's own summary, nothing after it" \
    "$verdict" "OK|Deleted 2, kept 1, errors 0"
check "reader, a real run: the pruner deleted the two" "$(deleted)" "$ASCII_OLD $HOME_OLD "

# Two targets: both summaries, in the pruner's order, "; " between them and
# nothing after the last. The detail was joined with `tr '\n' '; '`, which
# writes ";" alone and left the trim after it nothing to match: "a;b;".
fresh2
listing_for primary "$ASCII_OLD" "$HOME_OLD" "$FUTURE" "$OTHER"
listing_for primary2 "$DIR_OLD" "$FUTURE"
check "reader, two targets, a real run: both summaries, '; ' between them, nothing after the last" \
    "$(read_back real)" "OK|Deleted 2, kept 1, errors 0; Deleted 1, kept 1, errors 0"
check "reader, two targets, a dry run: both summaries, '; ' between them, nothing after the last" \
    "$(read_back dryrun)" "OK|Would delete 2, kept 1, errors 0; Would delete 1, kept 1, errors 0"

# A pruner that examined nothing prints no summary and exits 0: the reader must
# not call it OK, in either mode. The primary is not mounted; the other target
# is a mirror, skipped by design.
fresh
rm -f "$STATE/mounted/primary"
run_pruner C --dryrun
check "pruner, nothing mounted: exit status 0 and no summary (it is the reader that must decide)" \
    "$RC $(said 'Would delete') $(said 'Deleted')" "0 0 0"
for mode in dryrun real; do
    verdict="$(read_back "$mode")"
    check "reader, $mode with no summary: FAIL" "$(status_of "$verdict")" "FAIL"
    check "reader, $mode with no summary: the detail says so, in full" \
        "$verdict" "FAIL|exit 0 with no summary line; pruner may have examined nothing"
done
read_back dryrun >/dev/null
check "reader, a dry run with no summary: the warning names the dry run's shape" \
    "$(reader_said 'no per-target summary (Would delete N, kept N, errors N)')" "1"
read_back real >/dev/null
check "reader, a real run with no summary: the warning names the real run's shape" \
    "$(reader_said 'no per-target summary (Deleted N, kept N, errors N)')" "1"

# A summary of the other mode is not this mode's: a real run that ran dry, or a
# dry run that ran for real, did not do what it was asked.
fresh
listing "$ASCII_OLD" "$HOME_OLD" "$FUTURE" "$OTHER"
verdict="$(PRUNER_FORCE=dry read_back real)"
check "reader, a real run whose pruner ran dry: FAIL" "$(status_of "$verdict")" "FAIL"
check "reader, a real run whose pruner ran dry: nothing was deleted" "$(deleted)" ""
verdict="$(PRUNER_FORCE=real read_back dryrun)"
check "reader, a dry run whose pruner ran for real: FAIL" "$(status_of "$verdict")" "FAIL"

# A pruner that failed is a FAIL by its exit status, as before: no listing for
# the primary is a target not pruned, and the run says so.
fresh
for mode in dryrun real; do
    verdict="$(read_back "$mode")"
    check "reader, $mode, the pruner failed (no listing): FAIL by its exit status" "$verdict" "FAIL|exit code 1"
done

# ---------------------------------------------------------------------------
echo "== a target it cannot tell is mounted is NOT pruned, and the run fails (bd 2dmn)"
# ---------------------------------------------------------------------------
# `mountpoint -q ... 2>/dev/null` read every failure as "not mounted - nothing
# examined": exit 0, nothing said, and with any other target examined the run
# was OK. Not mounted (32) and a path that is not there stay quiet skips.
fresh
listing "$ASCII_OLD" "$HOME_OLD" "$FUTURE" "$OTHER"
probe_mode primary error
run_pruner C
check "probe error on the primary, real run: the pruner exits 1" "$RC" "1"
check "... says why, with mountpoint's own message and status" \
    "$(said "Could not tell whether $PRIMARY is mounted — mountpoint: $PRIMARY: Input/output error (exit 1)")" "1"
check "... says the target was not pruned" "$(said "$PRIMARY NOT pruned - state unknown")" "1"
check "... and the closing line counts it" \
    "$(said 'Boot archive cleanup FAILED: 0 deletion error(s), 1 target(s) not examined.')" "1"
check "... btrfs was never asked about it, nothing deleted, no summary printed for it" \
    "$(btrfs_asked primary) [$(deleted)] $(said 'Deleted')$(said 'Would delete')" "0 [] 00"
check "... the skip's own wording is not used" "$(said "Skipping $PRIMARY")" "0"
run_pruner C --dryrun
check "probe error on the primary, dry run: the pruner exits 1 too" "$RC" "1"
for mode in dryrun real; do
    check "reader, $mode, a target the pruner could not tell: FAIL by its exit status" \
        "$(read_back "$mode")" "FAIL|exit code 1"
done
read_back real >/dev/null
check "reader, the warning carries the pruner's own words" \
    "$(reader_said "Could not tell whether $PRIMARY is mounted — mountpoint: $PRIMARY: Input/output error (exit 1)")" "1"

fresh
probe_mode primary rc:2
run_pruner C
check "mountpoint exits 2 and says nothing: could not tell, exit 1" \
    "$RC $(said "Could not tell whether $PRIMARY is mounted — mountpoint printed nothing (exit 2)")" "1 1"
probe_mode primary rc:143
run_pruner C
check "mountpoint killed by TERM (143): could not tell, exit 1" \
    "$RC $(said "Could not tell whether $PRIMARY is mounted — mountpoint printed nothing (exit 143)")" "1 1"

# The quiet answers, unchanged: exit 0, the skip said, no failure.
fresh
rm -f "$STATE/mounted/primary"
run_pruner C
check "not a mount point (32): the pruner exits 0" "$RC" "0"
check "not a mount point: the skip is said, and nothing says it failed" \
    "$(said "Skipping $PRIMARY (not mounted - nothing examined)") $(said 'NOT pruned') $(said 'FAILED')" "1 0 0"
fresh
probe_mode primary absent
run_pruner C
check "a path that is not there (an absent drive's mount point): the pruner exits 0" "$RC" "0"
check "a path that is not there: the same quiet skip, nothing says it failed" \
    "$(said "Skipping $PRIMARY (not mounted - nothing examined)") $(said 'NOT pruned') $(said 'FAILED')" "1 0 0"

# No mountpoint program at all (exit 127): every target is one it cannot tell.
fresh
RUN_PATH="$SYSBIN" ALLOW_NOT_FOUND='mountpoint: command not found' run_pruner C
check "no mountpoint program: the pruner exits 1, and both targets are counted" \
    "$RC $(said 'Boot archive cleanup FAILED: 0 deletion error(s), 2 target(s) not examined.')" "1 1"
check "no mountpoint program: said with the shell's own words and the exit status" \
    "$(grep -c "Could not tell whether $PRIMARY is mounted — .*mountpoint: command not found (exit 127)" "$STATE/out")" "1"

# Every target, a mirror included: the probe comes before the role could matter,
# and one that fails for a single drive is an anomaly worth a failed run.
fresh
listing "$ASCII_OLD" "$HOME_OLD" "$FUTURE" "$OTHER"
probe_mode mirror error
run_pruner C
check "probe error on the mirror: the primary is pruned all the same" "$(deleted)" "$ASCII_OLD $HOME_OLD "
check "probe error on the mirror: and the run fails, naming it" \
    "$RC $(said "Could not tell whether $MIRROR is mounted") $(said 'Boot archive cleanup FAILED: 0 deletion error(s), 1 target(s) not examined.')" "1 1 1"

# One target it cannot tell about does not stop the others: the second primary
# is pruned and its summary printed, and the run still fails.
fresh2
listing_for primary "$ASCII_OLD" "$FUTURE"
listing_for primary2 "$HOME_OLD" "$FUTURE"
probe_mode primary error
run_pruner C
check "two primaries, the first cannot tell: nothing is deleted under it" \
    "$(grep -c "^$PRIMARY/" "$STATE/deleted" || true)" "0"
check "two primaries, the first cannot tell: the second's summary is printed" \
    "$(said 'Deleted 1, kept 1, errors 0')" "1"
check "two primaries, the first cannot tell: the run exits 1, one target not examined" \
    "$RC $(said 'Boot archive cleanup FAILED: 0 deletion error(s), 1 target(s) not examined.')" "1 1"
check "two primaries, the first cannot tell: btrfs was asked about the second only" \
    "$(btrfs_asked primary) $(btrfs_asked primary2)" "0 2"
check "two primaries, the first cannot tell: the second's expired archive is the one deleted" \
    "$(sed "s|^$PRIMARY2/||" "$STATE/deleted" | tr '\n' ' ')" "$HOME_OLD "

# The pruner's reading of the probe is the unmount gate's: a standalone script
# cannot source its sibling, so the two functions, and the two variables they
# set, are one text twice, and this fails if either copy changes alone.
for fn in probe_mount_point probe_state; do
    theirs="$(sed -n "/^$fn() {/,/^}/p" "$ROOT/scripts/backup-run.sh")"
    ours="$(sed -n "/^$fn() {/,/^}/p" "$SRC")"
    verdict=identical
    [[ -n "$theirs" && -n "$ours" ]] || verdict="missing from one of the scripts"
    [[ "$verdict" != identical || "$theirs" == "$ours" ]] || verdict=different
    check "$fn is the same text in backup-run.sh and boot-archive-cleanup.sh" "$verdict" "identical"
done
for line in 'PROBE_STATE="unknown"' 'PROBE_WHY="no mount probe has run"'; do
    check "the initial value $line is in both scripts" \
        "$(grep -cxF -- "$line" "$ROOT/scripts/backup-run.sh")$(grep -cxF -- "$line" "$SRC")" "11"
done
check "probe_mount_point is named in the pruner's code only by its definition and by probe_state" \
    "$(grep -v '^[[:space:]]*#' "$SRC" | grep -c 'probe_mount_point')" "2"

echo
echo "passed=$pass failed=$fail"
[[ $fail -eq 0 ]] || exit 1
[[ -z "$NOT_RUN" ]] || echo "NOT RUN: $NOT_RUN"
echo "BOOT ARCHIVE CLEANUP SUITE GREEN"
