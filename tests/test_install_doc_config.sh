#!/bin/bash
# The config docs/INSTALL.md shows for an installation without the wizard is one
# btrdasd accepts (bd DAS-Backup-Manager-x0m1).
#
# That section used to install the files, copy a btrbk.conf and enable the
# timers, and never create /etc/das-backup/config.toml — which backup-run.sh
# reads first, exiting 1 without it. It now has the reader write the config by
# hand and run `btrdasd setup --force`, so the config it shows must be one the
# parser takes, or the reader is stuck at the first step. This suite takes the
# ```toml block after the marker comment and runs the given btrdasd on it:
#   1. the marker is there once, and a toml block follows it;
#   2. `btrdasd config validate` accepts the block;
#   3. `btrdasd config dump-env` — what backup-run.sh reads — accepts it, with
#      one source and one target;
#   4. the same block without `version` is refused: the parser is not merely
#      accepting anything.
#
# Usage: test_install_doc_config.sh <btrdasd> [<INSTALL.md>]
# Registered with ctest as shell-install-doc-config when the build makes btrdasd.
# Writes only beneath a mktemp directory. No root, no network.

set -uo pipefail

if (($# < 1 || $# > 2)); then
    echo "usage: $0 <btrdasd> [<INSTALL.md>]" >&2
    exit 2
fi
btrdasd="$1"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
doc="${2:-$(dirname "$here")/docs/INSTALL.md}"
marker='<!-- tests/test_install_doc_config.sh'

if [[ ! -x "$btrdasd" ]]; then
    echo "FAIL: no btrdasd at $btrdasd" >&2
    exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
config="$work/config.toml"

fails=0
check() {
    local name="$1" got="$2" want="$3"
    if [[ "$got" == "$want" ]]; then
        echo "ok   $name"
    else
        echo "FAIL $name"
        echo "       got:  $got"
        echo "       want: $want"
        fails=$((fails + 1))
    fi
}

check "1. the marker is in $(basename "$doc") once" \
    "$(grep -cF "$marker" "$doc")" "1"
# The first ```toml block after the marker, without its fences.
awk -v m="$marker" '
    index($0, m) == 1 { seen = 1; next }
    seen && !open && /^```toml[[:space:]]*$/ { open = 1; next }
    open && /^```[[:space:]]*$/ { exit }
    open { print }
' "$doc" > "$config"
check "1. a toml block follows the marker" \
    "$([[ -s "$config" ]] && echo yes || echo no)" "yes"

out="$("$btrdasd" config validate --config "$config" 2>&1)"
check "2. config validate accepts it" "$?|$out" "0|Config is valid."

env_out="$("$btrdasd" config dump-env --config "$config" 2>&1)"
rc=$?
check "3. config dump-env accepts it" "$rc" "0"
check "3. one source and one target" \
    "$(grep -E '^DAS_(SOURCE|TARGET)_COUNT=' <<< "$env_out" | sort | paste -sd ' ' -)" \
    "DAS_SOURCE_COUNT=1 DAS_TARGET_COUNT=1"

grep -v '^version[[:space:]]*=' "$config" > "$work/no-version.toml"
out="$("$btrdasd" config validate --config "$work/no-version.toml" 2>&1)"
rc=$?
check "4. without version it is refused" \
    "$((rc != 0))|$(grep -cF "missing field \`version\`" <<< "$out")" "1|1"

if ((fails > 0)); then
    echo "INSTALL DOC CONFIG SUITE RED ($fails failed)"
    exit 1
fi
echo "INSTALL DOC CONFIG SUITE GREEN"
