#!/bin/bash
# tests/test_pacman_install.sh
#
# Regression test for DAS-Backup-Manager-ravr: CI's Arch container jobs install
# their packages through .github/scripts/pacman-install.sh, which retries a
# failed `pacman -Syu` up to 3 times instead of turning main red on the first
# mirror hiccup (CI run 37216474381: one TLS reset on a package's .sig file,
# before any build step ran; re-running the job passed).
#
# The script runs for real, executed by path as CI runs it, with a stub `pacman`
# (and, except in the one timed case, a stub `sleep`) first on PATH. Nothing here
# pulls a container image, installs a package, or touches the host's /etc: the
# script's two config files are redirected to a fixture tree by
# PACMAN_INSTALL_ROOT, a test seam the script refuses unless
# PACMAN_INSTALL_TESTING=1 is also set.
#
# Every property is shown in both directions: the script passes when it should
# (first try; recovery after one failure; recovery after a bad first mirror) and
# fails when it should (three failures exit nonzero, a missing wait or refresh
# or rotation, a refused seam). A check that only ever reports good news is not
# a check.
#
# To falsify the suite, point it at a deliberately broken copy:
#   PACMAN_INSTALL_SCRIPT=/path/to/mutant.sh bash tests/test_pacman_install.sh
# and it must report FAIL lines (it exits nonzero).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="${PACMAN_INSTALL_SCRIPT:-$REPO_ROOT/.github/scripts/pacman-install.sh}"
WORKFLOWS="$REPO_ROOT/.github/workflows"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

PACMAN_BIN="$WORK/pacman-bin"
SLEEP_BIN="$WORK/sleep-bin"
EMPTY_BIN="$WORK/empty-bin"
mkdir -p "$PACMAN_BIN" "$SLEEP_BIN" "$EMPTY_BIN"

PASSES=0
FAILS=0

pass() {
    PASSES=$((PASSES + 1))
    printf '  PASS: %s\n' "$1"
}

fail() {
    FAILS=$((FAILS + 1))
    printf '  FAIL: %s\n' "$1"
}

# expect LABEL EXPECTED ACTUAL
expect() {
    if [[ "$2" == "$3" ]]; then
        pass "$1"
    else
        fail "$1 (expected '$2', got '$3')"
    fi
}

# expect_contains LABEL HAYSTACK NEEDLE
expect_contains() {
    if [[ "$2" == *"$3"* ]]; then
        pass "$1"
    else
        fail "$1 (missing '$3')"
    fi
}

# Characters of other scripts, as bytes (bd DAS-Backup-Manager-1bsx): under
# en_US.UTF-8 bash's regex ranges [0-9] and [A-Za-z] match them. A case about
# that needs a locale where bash's own range does, or it could not fail: where
# it does not (C and C.UTF-8 show nothing; this host's CI container has no
# en_US) the case is NOT RUN, said at the end, never passed.
ARABIC_THREE=$'\xd9\xa3'
SUPERSCRIPT_TWO=$'\xc2\xb2'
E_ACUTE=$'\xc3\xa9'
NOT_RUN=""
# shellcheck disable=SC2030,SC2031  # the LC_ALL set in a probe stays in its subshell
locale_widens_digits() {
    (export LC_ALL=en_US.UTF-8; [[ $ARABIC_THREE =~ [0-9] ]]) 2>/dev/null # locale-range-ok: the probe
}
# shellcheck disable=SC2030,SC2031  # the LC_ALL set in a probe stays in its subshell
locale_widens_letters() {
    (export LC_ALL=en_US.UTF-8; [[ $E_ACUTE =~ [A-Za-z] ]]) 2>/dev/null # locale-range-ok: the probe
}

# ---------------------------------------------------------------------------
# The stubs. Quoted heredocs: nothing here is expanded by this shell.
# ---------------------------------------------------------------------------

# The stub fails with the real failure of CI run 37216474381 (the four stderr
# lines pacman 7.1.0 printed), naming the mirror that was first in the list at
# the time of the call. Which calls fail is a plan: one word per install call,
# the last word repeating -- ok | fail | bad:HOST (fails only while HOST is the
# first enabled mirror, i.e. one bad mirror that pacman's own failover cannot
# hide: the pinned-.sig case).
cat >"$PACMAN_BIN/pacman" <<'STUB'
#!/bin/bash
state=${STUB_STATE:?STUB_STATE is not set: this stub only runs under tests/test_pacman_install.sh}
root=${PACMAN_INSTALL_ROOT:?PACMAN_INSTALL_ROOT is not set}
first=$(awk -F/ '/^[[:space:]]*Server[[:space:]]*=/ { print $3; exit }' "$root/etc/pacman.d/mirrorlist")
case " $* " in
    *" -Syy "*) kind=refresh ;;
    *) kind=install ;;
esac
printf '%s\t%s\t%s\n' "$kind" "$*" "$first" >>"$state/calls"
if [[ $kind == refresh ]]; then
    if [[ -e $state/refresh-fails ]]; then
        echo "error: stub refresh failure" >&2
        exit 1
    fi
    exit 0
fi
n=$(awk -F'\t' '$1 == "install" { n++ } END { print n + 0 }' "$state/calls")
read -r -a plan <"$state/plan"
verdict=${plan[n - 1]:-${plan[${#plan[@]} - 1]}}
bad=0
case $verdict in
    ok) ;;
    fail) bad=1 ;;
    bad:*) if [[ $first == "${verdict#bad:}" ]]; then bad=1; fi ;;
    *) echo "stub: unknown verdict '$verdict'" >&2; exit 99 ;;
esac
if ((bad)); then
    # Failure injection: after the first failure the mirror list can no longer be
    # opened (a dangling link; a directory would open and read as empty).
    if [[ -e $state/break-list ]]; then
        rm -f "$root/etc/pacman.d/mirrorlist"
        ln -s "$state/gone" "$root/etc/pacman.d/mirrorlist"
    fi
    printf "error: failed retrieving file 'tslib-1.24-1-x86_64.pkg.tar.zst.sig' from %s : OpenSSL SSL_read: SSL_ERROR_SYSCALL, errno 0 [stub call %s]\n" "$first" "$n" >&2
    echo "warning: failed to retrieve some files" >&2
    echo "error: failed to commit transaction (failed to retrieve some files)" >&2
    echo "Errors occurred, no packages were upgraded." >&2
    exit 42
fi
echo ":: stub: installed (call $n, first mirror $first)"
STUB

cat >"$SLEEP_BIN/sleep" <<'STUB'
#!/bin/bash
printf '%s\n' "$*" >>"${STUB_STATE:?STUB_STATE is not set}/sleeps"
STUB

chmod +x "$PACMAN_BIN/pacman" "$SLEEP_BIN/sleep"

# ---------------------------------------------------------------------------
# Safety net: nothing below may reach the real pacman. `command -v` skips a file
# that is not executable, so a stub that lost its mode would resolve to
# /usr/bin/pacman -- and under root in CI that would be a real `pacman -Syu`.
# ---------------------------------------------------------------------------
resolved=$(env "PATH=$PACMAN_BIN:$PATH" bash -c 'command -v pacman')
if [[ "$resolved" != "$PACMAN_BIN/pacman" ]]; then
    echo "FAIL: 'pacman' resolves to '$resolved', not the stub; refusing to run" >&2
    exit 1
fi

if [[ ! -f "$SCRIPT" ]]; then
    echo "FAIL: cannot find $SCRIPT" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Per-case fixtures and helpers
# ---------------------------------------------------------------------------
CASE_N=0
CASE_FAILS_AT_START=0
CASE_NAME=""
CASE_DIR=""
ROOT=""
STATE=""
OUT=""
RC=0

# new_case NAME -- a fresh fixture tree shaped like the archlinux image's own:
# two pkgbuild.com mirrors enabled, ParallelDownloads on, repos reading the list.
new_case() {
    CASE_N=$((CASE_N + 1))
    CASE_NAME="$1"
    CASE_FAILS_AT_START=$FAILS
    CASE_DIR="$WORK/case-$CASE_N"
    ROOT="$CASE_DIR/root"
    STATE="$CASE_DIR/state"
    mkdir -p "$ROOT/etc/pacman.d" "$STATE" "$CASE_DIR/tmp"
    : >"$STATE/calls"
    : >"$STATE/sleeps"
    printf 'ok\n' >"$STATE/plan"
    # shellcheck disable=SC2016  # $repo and $arch are pacman's variables, literal here
    printf '%s\n' \
        'Server = https://fastly.mirror.pkgbuild.com/$repo/os/$arch' \
        'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch' \
        >"$ROOT/etc/pacman.d/mirrorlist"
    cat >"$ROOT/etc/pacman.conf" <<'EOF'
[options]
HoldPkg     = pacman glibc
Architecture = auto
NoProgressBar
VerbosePkgLists
ParallelDownloads = 5
DownloadUser = alpm
SigLevel    = Required DatabaseOptional

[core]
Include = /etc/pacman.d/mirrorlist

[extra]
Include = /etc/pacman.d/mirrorlist
EOF
    chmod 0644 "$ROOT/etc/pacman.conf" "$ROOT/etc/pacman.d/mirrorlist"
    printf '\n== %s ==\n' "$CASE_NAME"
}

# end_case -- on any failure in the case, show what the script printed.
end_case() {
    if ((FAILS > CASE_FAILS_AT_START)); then
        printf '  --- script output (exit %s) ---\n' "$RC"
        printf '%s\n' "$OUT" | sed 's/^/  | /'
        printf '  --- stub calls (kind, arguments, first mirror) ---\n'
        sed 's/^/  | /' "$STATE/calls"
    fi
}

# run_script [VAR=value ...] -- PACKAGE...   (sets OUT and RC; the real PATH
# minus nothing: stubs first). The three seams are scrubbed from the ambient
# environment so the case says exactly which ones it sets.
RUN_PATH=""
run_script() {
    local -a envs=()
    while (($# > 0)) && [[ "$1" != "--" ]]; do
        envs+=("$1")
        shift
    done
    shift
    RC=0
    OUT=$(env -u PACMAN_RETRY_WAITS -u PACMAN_INSTALL_ROOT -u PACMAN_INSTALL_TESTING \
        "STUB_STATE=$STATE" "TMPDIR=$CASE_DIR/tmp" \
        "PATH=${RUN_PATH:-$PACMAN_BIN:$SLEEP_BIN:$PATH}" \
        "${envs[@]}" "$SCRIPT" "$@" 2>&1) || RC=$?
}

# Entries the script left in the scratch directory it was given as TMPDIR.
leftovers() {
    find "$CASE_DIR/tmp" -mindepth 1 -printf '.' | wc -c
}

# run_std [extra VAR=value ...] -- PACKAGE...  : the test marker and the fixture root.
run_std() {
    local -a extra=()
    while (($# > 0)) && [[ "$1" != "--" ]]; do
        extra+=("$1")
        shift
    done
    run_script PACMAN_INSTALL_TESTING=1 "PACMAN_INSTALL_ROOT=$ROOT" "${extra[@]}" "$@"
}

count_kind() {
    awk -F'\t' -v k="$1" '$1 == k { n++ } END { print n + 0 }' "$STATE/calls"
}

kinds() {
    awk -F'\t' '{ printf "%s%s", sep, $1; sep = " " } END { print "" }' "$STATE/calls"
}

install_first_hosts() {
    awk -F'\t' '$1 == "install" { printf "%s%s", sep, $3; sep = " " } END { print "" }' "$STATE/calls"
}

first_install_args() {
    awk -F'\t' '$1 == "install" { print $2; exit }' "$STATE/calls"
}

refresh_args() {
    awk -F'\t' '$1 == "refresh" { printf "%s%s", sep, $2; sep = "|" } END { print "" }' "$STATE/calls"
}

sleeps() {
    awk '{ printf "%s%s", sep, $0; sep = " " } END { print "" }' "$STATE/sleeps"
}

# Independent readers of the fixture files (not the script's own parsing).
enabled_hosts() {
    awk -F/ '/^[[:space:]]*Server[[:space:]]*=/ { printf "%s%s", sep, $3; sep = " " } END { print "" }' "$ROOT/etc/pacman.d/mirrorlist"
}

# "<section>:<value>" for every active ParallelDownloads line, in file order.
active_parallel() {
    awk '/^[[:space:]]*\[/ { sec = $0; gsub(/[][ \t]/, "", sec); next }
         /^[[:space:]]*ParallelDownloads[[:space:]]*=/ {
             v = $0; sub(/^[^=]*=[[:space:]]*/, "", v)
             printf "%s%s:%s", sep, sec, v; sep = " "
         }
         END { print "" }' "$ROOT/etc/pacman.conf"
}

sum_of() {
    sha256sum <"$1"
}

mode_of() {
    stat -c %a "$1"
}

PKGS=(base-devel rust cmake extra-cmake-modules)
FASTLY=fastly.mirror.pkgbuild.com
GEO=geo.mirror.pkgbuild.com

echo "=== pacman-install.sh: $SCRIPT ==="

# ---------------------------------------------------------------------------
# Structure: executed by path, as every workflow step does.
# ---------------------------------------------------------------------------
new_case "structure: the script is executable bash with strict mode"
if [[ -x "$SCRIPT" ]]; then pass "script is executable (workflows run it by path)"; else fail "script is not executable"; fi
expect "first line is the bash shebang" '#!/bin/bash' "$(head -n 1 "$SCRIPT")"
if [[ "$(<"$SCRIPT")" == *"set -euo pipefail"* ]]; then pass "script sets -euo pipefail"; else fail "script lacks set -euo pipefail"; fi
end_case

# ---------------------------------------------------------------------------
# The three behaviours the brief names.
# ---------------------------------------------------------------------------
new_case "first try succeeds: one install, no refresh, no wait, no config change"
before_conf=$(sum_of "$ROOT/etc/pacman.conf")
before_list=$(sum_of "$ROOT/etc/pacman.d/mirrorlist")
run_std -- "${PKGS[@]}"
expect "exit status" 0 "$RC"
expect "pacman -Syu called exactly once" 1 "$(count_kind install)"
expect "no -Syy refresh" 0 "$(count_kind refresh)"
expect "no wait" "" "$(sleeps)"
expect "same command, same package list" "-Syu --noconfirm ${PKGS[*]}" "$(first_install_args)"
expect "pacman.conf untouched on an image-shaped tree" "$before_conf" "$(sum_of "$ROOT/etc/pacman.conf")"
expect "mirrorlist untouched on an image-shaped tree" "$before_list" "$(sum_of "$ROOT/etc/pacman.d/mirrorlist")"
expect "no scratch files left behind (success path)" 0 "$(leftovers)"
end_case

new_case "fails once, then succeeds: two installs with a refresh between, one 15 s wait"
printf 'fail ok\n' >"$STATE/plan"
run_std -- "${PKGS[@]}"
expect "exit status" 0 "$RC"
expect "call order: install, refresh, install" "install refresh install" "$(kinds)"
expect "the refresh is exactly pacman -Syy" "-Syy" "$(refresh_args)"
expect "the wait before attempt 2 is 15 s" "15" "$(sleeps)"
expect_contains "the failed attempt's stderr is in the log" "$OUT" "[stub call 1]"
if [[ "$OUT" == *"[stub call 2]"* ]]; then fail "attempt 2 succeeded yet printed a failure"; else pass "the recovered attempt printed no failure"; fi
end_case

new_case "fails every time: exactly 3 installs, 2 refreshes, waits 15 s then 30 s, nonzero, every reason logged"
printf 'fail\n' >"$STATE/plan"
run_std -- "${PKGS[@]}"
if ((RC != 0)); then pass "exit status is nonzero ($RC)"; else fail "exited 0 after every attempt failed"; fi
expect "exit status is the last attempt's own" 42 "$RC"
expect "pacman -Syu called exactly 3 times" 3 "$(count_kind install)"
expect "pacman -Syy called exactly twice" 2 "$(count_kind refresh)"
expect "call order" "install refresh install refresh install" "$(kinds)"
expect "waits are 15 s then 30 s, none after the last attempt" "15 30" "$(sleeps)"
expect "no scratch files left behind (failure path)" 0 "$(leftovers)"
for n in 1 2 3; do
    expect_contains "attempt $n's stderr is in the log" "$OUT" "[stub call $n]"
done
expect_contains "the reason itself, not just a count, is logged" "$OUT" "SSL_ERROR_SYSCALL"
# The closing summary must carry every attempt again, so the tail of a red step
# says why without scrolling.
summary="${OUT#*GAVE UP}"
if [[ "$summary" == "$OUT" ]]; then
    fail "no closing summary (no 'GAVE UP' line)"
else
    for n in 1 2 3; do
        expect_contains "the closing summary names attempt $n's reason" "$summary" "[stub call $n]"
    done
fi
end_case

# ---------------------------------------------------------------------------
# The waits really are waited, and the override is a test-only seam.
# ---------------------------------------------------------------------------
new_case "PACMAN_RETRY_WAITS is honoured under the test marker"
printf 'fail\n' >"$STATE/plan"
run_std PACMAN_RETRY_WAITS="0 0" -- "${PKGS[@]}"
expect "the overridden waits are the ones slept" "0 0" "$(sleeps)"
expect "still exactly 3 attempts" 3 "$(count_kind install)"
end_case

new_case "the waits really elapse (stub sleep removed): 1 s + 1 s measured on the clock"
printf 'fail fail ok\n' >"$STATE/plan"
RUN_PATH="$PACMAN_BIN:$PATH"
t0=$(date +%s.%N)
run_std PACMAN_RETRY_WAITS="1 1" -- "${PKGS[@]}"
t1=$(date +%s.%N)
RUN_PATH=""
elapsed=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.2f", b - a }')
expect "exit status" 0 "$RC"
expect "three installs" 3 "$(count_kind install)"
enough=$(awk -v e="$elapsed" 'BEGIN { print (e >= 2.0) ? "yes" : "no" }')
expect "at least 2.0 s elapsed (measured ${elapsed} s)" yes "$enough"
end_case

new_case "test seams are refused without the marker, before anything runs"
before_conf=$(sum_of "$ROOT/etc/pacman.conf")
before_list=$(sum_of "$ROOT/etc/pacman.d/mirrorlist")
run_script PACMAN_RETRY_WAITS="0 0" -- "${PKGS[@]}"
expect "PACMAN_RETRY_WAITS without the marker: exit status" 2 "$RC"
expect_contains "...and it says which variable and why" "$OUT" "PACMAN_RETRY_WAITS"
expect "...and pacman was never called" "" "$(kinds)"
run_script "PACMAN_INSTALL_ROOT=$ROOT" -- "${PKGS[@]}"
expect "PACMAN_INSTALL_ROOT without the marker: exit status" 2 "$RC"
expect_contains "...and it says which variable" "$OUT" "PACMAN_INSTALL_ROOT"
expect "...and pacman was never called" "" "$(kinds)"
run_script PACMAN_INSTALL_TESTING=0 PACMAN_RETRY_WAITS="0 0" -- "${PKGS[@]}"
expect "a marker that is not exactly 1 does not unlock the seam" 2 "$RC"
expect "no sleep was ever slept" "" "$(sleeps)"
expect "config files untouched by the refused runs (conf)" "$before_conf" "$(sum_of "$ROOT/etc/pacman.conf")"
expect "config files untouched by the refused runs (mirrorlist)" "$before_list" "$(sum_of "$ROOT/etc/pacman.d/mirrorlist")"
end_case

new_case "a malformed wait list is an error, never silently replaced by the default"
for bad in "0" "a b" "1 2 3" "-1 0" "1.5 2" ""; do
    run_std "PACMAN_RETRY_WAITS=$bad" -- "${PKGS[@]}"
    expect "PACMAN_RETRY_WAITS='$bad': exit status" 2 "$RC"
    expect "PACMAN_RETRY_WAITS='$bad': pacman never called" "" "$(kinds)"
done
# A whole number of seconds is ASCII digits: [0-9] in bash's regex also matches
# digits of other scripts and superscripts under en_US.UTF-8, which passed the
# guard and let the install run on a wait nobody can sleep.
if locale_widens_digits; then
    for bad in "$ARABIC_THREE $ARABIC_THREE" "$SUPERSCRIPT_TWO 0" "1 1$ARABIC_THREE"; do
        run_std "PACMAN_RETRY_WAITS=$bad" LC_ALL=en_US.UTF-8 -- "${PKGS[@]}"
        expect "en_US.UTF-8, PACMAN_RETRY_WAITS='$bad': exit status" 2 "$RC"
        expect "en_US.UTF-8, PACMAN_RETRY_WAITS='$bad': pacman never called" "" "$(kinds)"
    done
    run_std "PACMAN_RETRY_WAITS=0 0" LC_ALL=en_US.UTF-8 -- "${PKGS[@]}"
    expect "en_US.UTF-8, ASCII waits '0 0': exit status" 0 "$RC"
else
    NOT_RUN+="${NOT_RUN:+; }wait guard: bash's [0-9] matches no non-ASCII digit here (en_US.UTF-8 missing?)"
fi
end_case

new_case "usage: no packages is an error; pacman missing from PATH is an error"
run_std --
expect "no packages: exit status" 2 "$RC"
expect "no packages: pacman never called" "" "$(kinds)"
RUN_PATH="$EMPTY_BIN"
run_std -- "${PKGS[@]}"
RUN_PATH=""
expect "no pacman on PATH: exit status" 127 "$RC"
expect_contains "no pacman on PATH: says so" "$OUT" "pacman not found"
end_case

# ---------------------------------------------------------------------------
# One bad mirror must not turn the run red.
# ---------------------------------------------------------------------------
new_case "one bad first mirror: the retry starts on a different mirror and succeeds"
printf 'bad:%s\n' "$FASTLY" >"$STATE/plan"
run_std -- "${PKGS[@]}"
expect "exit status" 0 "$RC"
expect "call order: install, refresh, install" "install refresh install" "$(kinds)"
expect "attempt 1 led with the bad mirror, attempt 2 with the next one" "$FASTLY $GEO" "$(install_first_hosts)"
expect "the list now leads with the good mirror, the bad one last" "$GEO $FASTLY" "$(enabled_hosts)"
expect_contains "the log names the mirror that led attempt 1" "$OUT" "attempt 1/3, first mirror $FASTLY"
expect_contains "the log names the mirror that led attempt 2" "$OUT" "attempt 2/3, first mirror $GEO"
end_case

new_case "a reorder that fails is reported and does not stop the retry"
printf 'fail ok\n' >"$STATE/plan"
: >"$STATE/break-list"
run_std -- "${PKGS[@]}"
expect "exit status: the retry still ran, and passed" 0 "$RC"
expect "call order" "install refresh install" "$(kinds)"
expect_contains "the failed reorder is in the log" "$OUT" "could not reorder"
end_case

# ---------------------------------------------------------------------------
# A refresh that fails does not decide the run; the install attempt does.
# ---------------------------------------------------------------------------
new_case "a failed -Syy is logged and does not abort the retry"
printf 'fail ok\n' >"$STATE/plan"
: >"$STATE/refresh-fails"
run_std -- "${PKGS[@]}"
expect "exit status: attempt 2 decides, and it passed" 0 "$RC"
expect "call order" "install refresh install" "$(kinds)"
expect_contains "the refresh failure is visible in the log" "$OUT" "stub refresh failure"
end_case

new_case "a failed -Syy never turns exhausted attempts into a pass"
printf 'fail\n' >"$STATE/plan"
: >"$STATE/refresh-fails"
run_std -- "${PKGS[@]}"
expect "exit status" 42 "$RC"
expect "still exactly 3 installs" 3 "$(count_kind install)"
end_case

# ---------------------------------------------------------------------------
# The mirror list: at least 2 hosts, added only when short, never reordered by
# the guard itself.
# ---------------------------------------------------------------------------
new_case "mirror guard: one host enabled (the other commented out) gains the second"
# shellcheck disable=SC2016  # pacman variables, literal here
printf '%s\n' \
    'Server = https://fastly.mirror.pkgbuild.com/$repo/os/$arch' \
    '#Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch' \
    >"$ROOT/etc/pacman.d/mirrorlist"
run_std -- "${PKGS[@]}"
expect "exit status" 0 "$RC"
expect "both Arch-run hosts are enabled, the original first" "$FASTLY $GEO" "$(enabled_hosts)"
expect "the commented line is left as it was" 1 "$(awk '/^#Server = https:\/\/geo\.mirror/ { n++ } END { print n + 0 }' "$ROOT/etc/pacman.d/mirrorlist")"
expect "the file keeps its mode" 644 "$(mode_of "$ROOT/etc/pacman.d/mirrorlist")"
end_case

new_case "mirror guard: the stock all-commented list gains both"
# shellcheck disable=SC2016  # pacman variables, literal here
printf '%s\n' \
    '## Worldwide' \
    '#Server = https://mirror.example.org/archlinux/$repo/os/$arch' \
    '#Server = https://other.example.net/archlinux/$repo/os/$arch' \
    >"$ROOT/etc/pacman.d/mirrorlist"
run_std -- "${PKGS[@]}"
expect "exit status" 0 "$RC"
expect "two enabled hosts" "$FASTLY $GEO" "$(enabled_hosts)"
before_list=$(sum_of "$ROOT/etc/pacman.d/mirrorlist")
run_std -- "${PKGS[@]}"
expect "a second run changes nothing (idempotent)" "$before_list" "$(sum_of "$ROOT/etc/pacman.d/mirrorlist")"
end_case

new_case "mirror guard: two lines on one host are one host, not two"
# shellcheck disable=SC2016  # pacman variables, literal here
printf '%s\n' \
    'Server = https://fastly.mirror.pkgbuild.com/$repo/os/$arch' \
    'Server = https://fastly.mirror.pkgbuild.com/other/$repo/os/$arch' \
    >"$ROOT/etc/pacman.d/mirrorlist"
run_std -- "${PKGS[@]}"
expect "a second HOST was added" "$FASTLY $FASTLY $GEO" "$(enabled_hosts)"
end_case

new_case "mirror guard: a URL scheme is ASCII — letters, digits, + - . — and nothing else"
# The scheme of a Server line starts with a letter and goes on with letters,
# digits, "+", "-" and ".". The control line uses every one of those, so a
# class that lost one would read it as no host and add a mirror. The other line
# has an accented letter for a scheme: under en_US.UTF-8 bash's regex [A-Za-z]
# also matches it, so it counted as a second host and no mirror was added.
# shellcheck disable=SC2016  # pacman variables, literal here
printf '%s\n' \
    'Server = https://fastly.mirror.pkgbuild.com/$repo/os/$arch' \
    'Server = a1+b-c.d://control.example.net/$repo/os/$arch' \
    >"$ROOT/etc/pacman.d/mirrorlist"
run_std LC_ALL=C -- "${PKGS[@]}"
expect "ASCII scheme with + - . and a digit: a host, so nothing is added" "$FASTLY control.example.net" "$(enabled_hosts)"
if locale_widens_letters; then
    # shellcheck disable=SC2016  # pacman variables, literal here
    printf '%s\n' \
        'Server = https://fastly.mirror.pkgbuild.com/$repo/os/$arch' \
        "Server = ${E_ACUTE}s://accented.example.net"'/$repo/os/$arch' \
        >"$ROOT/etc/pacman.d/mirrorlist"
    run_std LC_ALL=en_US.UTF-8 -- "${PKGS[@]}"
    expect "en_US.UTF-8, an accented scheme is no host: the second mirror is added" \
        "$FASTLY accented.example.net $GEO" "$(enabled_hosts)"
else
    NOT_RUN+="${NOT_RUN:+; }scheme class: bash's [A-Za-z] matches no non-ASCII letter here (en_US.UTF-8 missing?)"
fi
end_case

new_case "mirror guard: a list with no final newline gets its addition on its own line"
# shellcheck disable=SC2016  # pacman variables, literal here
printf '%s' 'Server = https://fastly.mirror.pkgbuild.com/$repo/os/$arch' >"$ROOT/etc/pacman.d/mirrorlist"
run_std -- "${PKGS[@]}"
expect "exit status" 0 "$RC"
expect "both hosts parse" "$FASTLY $GEO" "$(enabled_hosts)"
# Parsing the host alone cannot see text glued onto the end of the old last line.
strict=$(awk '/^Server = https:\/\/[^\/]+\/\$repo\/os\/\$arch$/ { n++ } END { print n + 0 }' "$ROOT/etc/pacman.d/mirrorlist")
expect "both Server lines are whole, clean lines: nothing was glued onto the old last line" 2 "$strict"
end_case

new_case "mirror guard: an unreadable mirror list is an error, and nothing is installed"
rm -f "$ROOT/etc/pacman.d/mirrorlist"
run_std -- "${PKGS[@]}"
if ((RC != 0)); then pass "exit status is nonzero ($RC)"; else fail "exited 0 with no mirror list"; fi
expect "pacman never called" "" "$(kinds)"
end_case

# ---------------------------------------------------------------------------
# pacman.conf: ParallelDownloads on, added only when absent, kept inside
# [options], file mode kept (makepkg reads this file as a non-root user).
# ---------------------------------------------------------------------------
new_case "parallel downloads: only commented out -> one active line, inside [options]"
cat >"$ROOT/etc/pacman.conf" <<'EOF'
[options]
HoldPkg     = pacman glibc
#ParallelDownloads = 5
SigLevel    = Required DatabaseOptional

[core]
Include = /etc/pacman.d/mirrorlist
EOF
chmod 0644 "$ROOT/etc/pacman.conf"
run_std -- "${PKGS[@]}"
expect "exit status" 0 "$RC"
expect "exactly one active ParallelDownloads = 5, in [options]" "options:5" "$(active_parallel)"
expect "pacman.conf keeps mode 644 (makepkg reads it as non-root)" 644 "$(mode_of "$ROOT/etc/pacman.conf")"
expect "the rest of the file is intact" 1 "$(awk '/^HoldPkg     = pacman glibc$/ { n++ } END { print n + 0 }' "$ROOT/etc/pacman.conf")"
end_case

new_case "parallel downloads: never mentioned -> added inside [options]"
cat >"$ROOT/etc/pacman.conf" <<'EOF'
[options]
HoldPkg     = pacman glibc

[core]
Include = /etc/pacman.d/mirrorlist
EOF
run_std -- "${PKGS[@]}"
expect "exit status" 0 "$RC"
expect "one active line, in [options]" "options:5" "$(active_parallel)"
end_case

new_case "parallel downloads: an operator's own value is left alone"
cat >"$ROOT/etc/pacman.conf" <<'EOF'
[options]
ParallelDownloads = 8

[core]
Include = /etc/pacman.d/mirrorlist
EOF
before_conf=$(sum_of "$ROOT/etc/pacman.conf")
run_std -- "${PKGS[@]}"
expect "exit status" 0 "$RC"
expect "file byte-identical" "$before_conf" "$(sum_of "$ROOT/etc/pacman.conf")"
end_case

new_case "parallel downloads: a pacman.conf with no [options] is an error, and nothing is installed"
cat >"$ROOT/etc/pacman.conf" <<'EOF'
[core]
Include = /etc/pacman.d/mirrorlist
EOF
run_std -- "${PKGS[@]}"
if ((RC != 0)); then pass "exit status is nonzero ($RC)"; else fail "exited 0 though ParallelDownloads could not be set"; fi
expect "pacman never called" "" "$(kinds)"
end_case

# ---------------------------------------------------------------------------
# Wiring: the producer is only useful if every consumer calls it. A workflow
# step that installs with a bare pacman bypasses the retry and is the next red
# main; and an Arch job that does not call the script has no retry at all.
# ---------------------------------------------------------------------------
new_case "wiring: every Arch container job installs through the script"
bare=$(awk '!/^[[:space:]]*#/ && /pacman[[:space:]]+-S/ { printf "%s:%s: %s\n", FILENAME, FNR, $0 }' "$WORKFLOWS"/*.yml)
if [[ -z "$bare" ]]; then
    pass "no workflow step runs a bare pacman -S"
else
    fail "bare pacman install in a workflow (bypasses the retry): $bare"
fi
arch_jobs=$(awk '!/^[[:space:]]*#/ && /archlinux:/ { n++ } END { print n + 0 }' "$WORKFLOWS"/*.yml)
wrapper_calls=$(awk '!/^[[:space:]]*#/ && /\.github\/scripts\/pacman-install\.sh/ { n++ } END { print n + 0 }' "$WORKFLOWS"/*.yml)
if ((arch_jobs > 0)); then pass "the workflows have Arch container jobs ($arch_jobs found)"; else fail "no Arch container job found: the wiring check would pass for the wrong reason"; fi
expect "Arch container jobs == calls to the script" "$arch_jobs" "$wrapper_calls"
end_case

printf '\n'
if [[ -n "$NOT_RUN" ]]; then
    echo "NOT RUN: $NOT_RUN"
fi
if ((FAILS == 0)); then
    echo "OK — $PASSES checks passed: pacman-install.sh retries boundedly, rotates a bad first mirror, and never passes on exhausted attempts."
    exit 0
fi
echo "FAILED — $FAILS of $((PASSES + FAILS)) checks failed." >&2
exit 1
