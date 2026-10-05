#!/bin/bash
# pacman-install.sh — install packages in an Arch container, retrying boundedly.
#
# Usage: .github/scripts/pacman-install.sh PACKAGE...
#
# Why. CI installs its toolchain with pacman inside the archlinux container, and
# connections reset by a mirror failed the whole step before any build ran (CI
# run 37216474381, bd DAS-Backup-Manager-ravr). Two files failed; one of them:
#
#   error: failed retrieving file 'tslib-1.24-1-x86_64.pkg.tar.zst.sig'
#          from fastly.mirror.pkgbuild.com : OpenSSL SSL_read: SSL_ERROR_SYSCALL, errno 0
#   Errors occurred, no packages were upgraded.
#
# Re-running the job passed. This script is that re-run, automatic and bounded.
#
# What, in order:
#   1. Two guards, no-ops on an image that already satisfies them: the mirror
#      list holds at least 2 distinct hosts, and pacman.conf has ParallelDownloads
#      on. A short list is topped up with Arch's own mirrors behind whatever is
#      there; nothing is ever removed or reordered by a guard.
#   2. `pacman -Syu --noconfirm PACKAGE...`, up to 3 attempts, waiting 15 s and
#      then 30 s between them.
#   3. Before each retry the mirror that was first moves to the end of the list
#      and `pacman -Syy` re-reads the databases from the new first one. Without
#      the move the retry begins on the very mirror that just failed: pacman
#      reaches a second server for a file only after the first has failed it, and
#      a package's .sig is requested from the URL its package came from with no
#      second server at all (libalpm 7.1.0, lib/libalpm/dload.c).
#   4. Each attempt's stderr is printed when the attempt ends. If all attempts
#      fail, a closing summary repeats each one and the script exits with the
#      last attempt's own status: it exits 0 only when an attempt did.
#
# Exit status: 0 installed; 1 a guard could not be satisfied (nothing was
# installed); 2 bad invocation or a refused test seam (nothing ran); 127 no
# pacman; otherwise pacman's status from the last attempt.
#
# Test seams, for tests/test_pacman_install.sh only. Both are REFUSED (exit 2)
# unless PACMAN_INSTALL_TESTING=1 is also set, because a CI step that set either
# would quietly shorten the backoff or redirect the config edits:
#   PACMAN_INSTALL_ROOT   prefix for /etc/pacman.conf and /etc/pacman.d/mirrorlist
#   PACMAN_RETRY_WAITS    the two waits in seconds, e.g. "0 0"

set -euo pipefail

readonly ATTEMPTS=3
readonly -a DEFAULT_WAITS=(15 30) # seconds before attempt 2, then before attempt 3
readonly MIN_MIRROR_HOSTS=2
readonly PARALLEL_DOWNLOADS=5
# $repo and $arch are pacman's own variables, expanded by pacman, not by this shell.
# shellcheck disable=SC2016
readonly -a ARCH_MIRRORS=(
    'https://fastly.mirror.pkgbuild.com/$repo/os/$arch'
    'https://geo.mirror.pkgbuild.com/$repo/os/$arch'
)
readonly SERVER_LINE_RE='^[[:space:]]*Server[[:space:]]*='
readonly SERVER_HOST_RE='^[[:space:]]*Server[[:space:]]*=[[:space:]]*[A-Za-z][A-Za-z0-9+.-]*://([^/[:space:]]+)'
readonly PARALLEL_LINE_RE='^[[:space:]]*ParallelDownloads[[:space:]]*='

WAITS=()
HOSTS=()
SCRATCH=""

log() {
    printf 'pacman-install: %s\n' "$*" >&2
}

# die STATUS MESSAGE...
die() {
    local status=$1
    shift
    log "$*"
    exit "$status"
}

check_test_seams() {
    local seam
    if [[ "${PACMAN_INSTALL_TESTING:-}" == 1 ]]; then
        return 0
    fi
    for seam in PACMAN_RETRY_WAITS PACMAN_INSTALL_ROOT; do
        if [[ -n "${!seam+x}" ]]; then
            die 2 "$seam is a test seam and PACMAN_INSTALL_TESTING=1 is not set; refusing to run"
        fi
    done
}

# Fill WAITS: the defaults, or the test seam. Exactly one whole number of
# seconds per gap between attempts; anything else is an error, never a default.
load_waits() {
    local w
    WAITS=("${DEFAULT_WAITS[@]}")
    if [[ -n "${PACMAN_RETRY_WAITS+x}" ]]; then
        read -r -a WAITS <<<"$PACMAN_RETRY_WAITS"
    fi
    if ((${#WAITS[@]} != ATTEMPTS - 1)); then
        die 2 "need $((ATTEMPTS - 1)) waits in seconds, got ${#WAITS[@]}: '${WAITS[*]}'"
    fi
    for w in "${WAITS[@]}"; do
        if [[ ! "$w" =~ ^[0-9]+$ ]]; then
            die 2 "wait '$w' is not a whole number of seconds"
        fi
    done
}

# load_hosts LIST: fill HOSTS with the distinct host of each enabled Server line,
# in file order. pacman counts a server's errors per host, not per line.
load_hosts() {
    local line="" host=""
    local -A seen=()
    HOSTS=()
    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ "$line" =~ $SERVER_HOST_RE ]]; then
            host=${BASH_REMATCH[1]}
            if [[ -z "${seen[$host]:-}" ]]; then
                seen[$host]=1
                HOSTS+=("$host")
            fi
        fi
    done <"$1"
}

# ensure_mirror_hosts LIST: at least MIN_MIRROR_HOSTS distinct hosts enabled.
# Appends Arch's own mirrors behind the existing ones when short; touches nothing otherwise.
ensure_mirror_hosts() {
    local list=$1 url host
    local -a add=()
    local -A have=()
    if [[ ! -r "$list" ]]; then
        die 1 "cannot read $list"
    fi
    load_hosts "$list"
    for host in "${HOSTS[@]}"; do
        have[$host]=1
    done
    for url in "${ARCH_MIRRORS[@]}"; do
        if ((${#HOSTS[@]} + ${#add[@]} >= MIN_MIRROR_HOSTS)); then
            break
        fi
        host=${url#*://}
        host=${host%%/*}
        if [[ -n "${have[$host]:-}" ]]; then
            continue
        fi
        have[$host]=1
        add+=("$url")
    done
    if ((${#HOSTS[@]} + ${#add[@]} < MIN_MIRROR_HOSTS)); then
        die 1 "$list has ${#HOSTS[@]} enabled host(s) and the built-in mirrors cannot make $MIN_MIRROR_HOSTS"
    fi
    if ((${#add[@]} > 0)); then
        log "$list has ${#HOSTS[@]} enabled host(s); adding ${#add[@]} Arch-run mirror(s) behind them"
        {
            printf '\n# Added by pacman-install.sh: fewer than %d mirror hosts were enabled.\n' "$MIN_MIRROR_HOSTS"
            printf 'Server = %s\n' "${add[@]}"
        } >>"$list"
        load_hosts "$list"
        if ((${#HOSTS[@]} < MIN_MIRROR_HOSTS)); then
            die 1 "$list still has only ${#HOSTS[@]} enabled host(s) after adding mirrors"
        fi
    fi
}

# ensure_parallel_downloads CONF: an active ParallelDownloads inside [options].
# An existing active value is the operator's and is left alone. sed -i keeps the
# file's mode, which matters: makepkg reads pacman.conf as a non-root user.
ensure_parallel_downloads() {
    local conf=$1 rc=0
    if [[ ! -r "$conf" ]]; then
        die 1 "cannot read $conf"
    fi
    grep -Eq "$PARALLEL_LINE_RE" "$conf" || rc=$?
    case $rc in
        0) return 0 ;;
        1) ;;
        *) die 1 "cannot search $conf (grep exited $rc)" ;;
    esac
    log "$conf has no active ParallelDownloads; setting $PARALLEL_DOWNLOADS inside [options]"
    sed -i "/^\[options\]/a ParallelDownloads = $PARALLEL_DOWNLOADS" "$conf"
    rc=0
    grep -Eq "$PARALLEL_LINE_RE" "$conf" || rc=$?
    if ((rc != 0)); then
        die 1 "$conf has no [options] section to hold ParallelDownloads"
    fi
}

# rotate_mirrors LIST: move the first enabled Server line to the end of the file.
# Called as an `if` condition, where bash ignores set -e inside the whole
# function: the read returns its failure explicitly, and the final write's own
# status is the function's.
rotate_mirrors() {
    local list=$1 line first="" moved=0
    local -a lines out=()
    mapfile -t lines <"$list" || return 1
    for line in "${lines[@]}"; do
        if ((moved == 0)) && [[ "$line" =~ $SERVER_LINE_RE ]]; then
            first=$line
            moved=1
            continue
        fi
        out+=("$line")
    done
    if ((moved == 0)); then
        return 0
    fi
    printf '%s\n' "${out[@]}" "$first" >"$list"
}

# show_tail FILE: the last 12 lines of FILE, indented, on stderr.
show_tail() {
    local i from
    local -a lines
    mapfile -t lines <"$1"
    if ((${#lines[@]} == 0)); then
        printf '    (no stderr output)\n' >&2
        return 0
    fi
    from=$((${#lines[@]} > 12 ? ${#lines[@]} - 12 : 0))
    for ((i = from; i < ${#lines[@]}; i++)); do
        printf '    %s\n' "${lines[i]}" >&2
    done
}

main() {
    local root list conf attempt pause rc=0 refresh_rc
    local -a codes=()

    if (($# == 0)); then
        die 2 "usage: pacman-install.sh PACKAGE..."
    fi
    check_test_seams
    load_waits
    if ! command -v pacman >/dev/null; then
        die 127 "pacman not found: this script is for an Arch container"
    fi

    root=${PACMAN_INSTALL_ROOT:-}
    conf="$root/etc/pacman.conf"
    list="$root/etc/pacman.d/mirrorlist"
    ensure_mirror_hosts "$list"
    ensure_parallel_downloads "$conf"

    SCRATCH=$(mktemp -d)
    trap 'rm -rf "$SCRATCH"' EXIT

    for ((attempt = 1; attempt <= ATTEMPTS; attempt++)); do
        log "attempt $attempt/$ATTEMPTS, first mirror ${HOSTS[0]}: pacman -Syu --noconfirm $*"
        rc=0
        pacman -Syu --noconfirm "$@" 2>"$SCRATCH/attempt-$attempt.err" || rc=$?
        cat "$SCRATCH/attempt-$attempt.err" >&2
        if ((rc == 0)); then
            log "attempt $attempt/$ATTEMPTS: installed"
            return 0
        fi
        codes[attempt]=$rc
        log "attempt $attempt/$ATTEMPTS FAILED (pacman exited $rc)"
        if ((attempt < ATTEMPTS)); then
            pause=${WAITS[attempt - 1]}
            log "waiting $pause s, then moving the first mirror to the end and re-reading the databases"
            sleep "$pause"
            if rotate_mirrors "$list"; then
                load_hosts "$list"
            else
                log "WARNING: could not reorder $list; the retry starts on the same mirror"
            fi
            refresh_rc=0
            pacman -Syy || refresh_rc=$?
            if ((refresh_rc != 0)); then
                log "WARNING: pacman -Syy exited $refresh_rc; the next attempt decides"
            fi
        fi
    done

    log "GAVE UP after $ATTEMPTS attempts; every attempt's reason follows (its last 12 stderr lines)"
    for ((attempt = 1; attempt <= ATTEMPTS; attempt++)); do
        log "attempt $attempt/$ATTEMPTS, pacman exited ${codes[attempt]}:"
        show_tail "$SCRATCH/attempt-$attempt.err"
    done
    return "$rc"
}

main "$@"
