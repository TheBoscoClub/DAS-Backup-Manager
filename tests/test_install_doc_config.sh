#!/bin/bash
# What docs/INSTALL.md shows for an installation without the wizard works
# (bd DAS-Backup-Manager-x0m1).
#
# That section used to install the files, copy a btrbk.conf and enable the
# timers, and never create /etc/das-backup/config.toml — which backup-run.sh
# reads first, exiting 1 without it. It now has the reader write the config by
# hand and run `btrdasd setup --force`, so the config it shows must be one the
# parser takes and one that does what it says, and the steps must be ones that
# can be followed on a host that has never run setup, or the reader is stuck.
# This suite takes the ```toml block after one marker comment and the ```bash
# block after another, and runs the given btrdasd on the first:
#   1. each marker is there once, and a block follows it;
#   2. `btrdasd config validate` accepts the config;
#   3. `btrdasd config dump-env` — what backup-run.sh reads — accepts it, with
#      one source and one target;
#   4. the same config without `version` is refused: the parser is not merely
#      accepting anything;
#   5. every file the steps edit by hand (sudoedit, tee) is in a directory an
#      earlier step creates (install -d, mkdir -p) or every host has. Installing
#      writes nothing under /etc, and sudoedit will not create a missing
#      directory (sudo 1.9.5 on), so a step that edits /etc/das-backup/config.toml
#      with nothing creating /etc/das-backup stops the reader at once. The check
#      is shown to refuse such steps: the doc's own with its creating step left
#      out, and constructed cases;
#   6. the config schedules no scrub of a label it does not define.
#      `[scrub].targets` defaults to the author's three labels and the scrub to
#      on, so a config that leaves the section out schedules a monthly scrub of
#      drives it never names, and scrubs none of its own. The check is shown to
#      refuse the config without its [scrub] section.
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
marker_config='<!-- tests/test_install_doc_config.sh validates '
marker_steps='<!-- tests/test_install_doc_config.sh checks '

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

# The first ```<lang> block after the line that starts with a marker, without
# its fences.
block_after() {
    awk -v m="$1" -v lang="$2" '
        index($0, m) == 1 { seen = 1; next }
        seen && !open && $0 ~ ("^```" lang "[[:space:]]*$") { open = 1; next }
        open && /^```[[:space:]]*$/ { exit }
        open { print }
    ' "$doc"
}

# Every file the steps on stdin edit by hand — a `sudoedit` or `tee` argument —
# as "made <file>" when its directory is one every host has or an earlier step
# created (`install -d` or `mkdir -p`, naming it or a directory below it), and
# "unmade <file>" when nothing did. Steps are read in order: a directory made
# after the edit does not count.
edits() {
    awk '
        function parent(p) { sub(/\/[^\/]*$/, "", p); return p == "" ? "/" : p }
        BEGIN {
            n = split("/ /etc /usr /var /opt /srv /tmp /home", base, " ")
            for (i = 1; i <= n; i++) made[base[i]] = 1
        }
        {
            sub(/[[:space:]]*#.*$/, "")
            n = split($0, t, /[[:space:]]+/)
            makes = 0; edit = 0
            for (i = 1; i <= n; i++) {
                if (t[i] == "install")
                    for (j = i + 1; j <= n; j++)
                        if (t[j] == "-d" || t[j] == "--directory") makes = 1
                if (t[i] == "mkdir")
                    for (j = i + 1; j <= n; j++)
                        if (t[j] == "-p" || t[j] == "--parents") makes = 1
                if (!edit && (t[i] == "sudoedit" || t[i] == "tee")) edit = i
            }
            if (makes)
                for (i = 1; i <= n; i++)
                    if (t[i] ~ /^\//)
                        for (d = t[i]; d != "/"; d = parent(d)) made[d] = 1
            if (edit) {
                # The file is the first absolute argument after the command.
                f = ""
                for (i = edit + 1; i <= n && f == ""; i++) if (t[i] ~ /^\//) f = t[i]
                if (f != "") print ((parent(f) in made) ? "made" : "unmade"), f
            }
        }'
}

# The labels a config's scrub is to scrub that no [[target]] of it defines, from
# `btrdasd config dump-env` on stdin; none when the scrub is off.
undefined_scrub_labels() {
    awk '
        function value(line) { sub(/^[^=]*=/, "", line); gsub(/\047/, "", line); return line }
        /^DAS_SCRUB_ENABLED=/ { enabled = value($0) }
        /^DAS_SCRUB_TARGETS=/ { n = split(value($0), want, " ") }
        /^DAS_TARGET_[0-9]+_LABEL=/ { have[value($0)] = 1 }
        END { if (enabled == "true") for (i = 1; i <= n; i++) if (!(want[i] in have)) print want[i] }
    ' | paste -sd ' ' -
}

check "1. the config marker is in $(basename "$doc") once" \
    "$(grep -cF "$marker_config" "$doc")" "1"
check "1. the steps marker is in $(basename "$doc") once" \
    "$(grep -cF "$marker_steps" "$doc")" "1"
block_after "$marker_config" toml > "$config"
check "1. a toml block follows the config marker" \
    "$([[ -s "$config" ]] && echo yes || echo no)" "yes"
steps="$(block_after "$marker_steps" bash)"
check "1. a bash block follows the steps marker" \
    "$([[ -n "$steps" ]] && echo yes || echo no)" "yes"

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

# The doc's steps.
check "5. the steps edit a file by hand" \
    "$(($(edits <<< "$steps" | grep -c .) >= 1))" "1"
check "5. every file they edit is in a directory they or every host have" \
    "$(edits <<< "$steps" | grep '^unmade ' | paste -sd '|' -)" ""
check "5. without the step that creates its directory, the edit is refused" \
    "$(grep -vE 'install -d|mkdir -p' <<< "$steps" | edits | paste -sd '|' -)" \
    "unmade /etc/das-backup/config.toml"
# The check itself: it must be able to say both, in each way a step can be wrong.
check "5. install -d covers the directory" \
    "$(printf 'sudo install -d -m 0755 /etc/das-backup\nsudoedit /etc/das-backup/config.toml\n' | edits)" \
    "made /etc/das-backup/config.toml"
check "5. mkdir -p covers the directory" \
    "$(printf 'sudo mkdir -p /etc/das-backup\nsudo tee /etc/das-backup/x < /dev/null\n' | edits)" \
    "made /etc/das-backup/x"
check "5. mkdir -p of a directory below covers it" \
    "$(printf 'sudo mkdir -p /etc/das-backup/sub\nsudoedit /etc/das-backup/x\n' | edits)" \
    "made /etc/das-backup/x"
check "5. a directory made after the edit does not cover it" \
    "$(printf 'sudoedit /etc/das-backup/x\nsudo install -d /etc/das-backup\n' | edits)" \
    "unmade /etc/das-backup/x"
check "5. another directory does not cover it" \
    "$(printf 'sudo install -d /etc/other\nsudoedit /etc/das-backup/x\n' | edits)" \
    "unmade /etc/das-backup/x"
check "5. install -p is not a step that makes a directory" \
    "$(printf 'sudo install -p a /etc/das-backup/a\nsudoedit /etc/das-backup/x\n' | edits)" \
    "unmade /etc/das-backup/x"
check "5. a comment names no edit" \
    "$(printf '# sudoedit /etc/das-backup/x\n' | edits | paste -sd '|' -)" ""

# The doc's config.
check "6. dump-env reports the scrub" \
    "$(grep -cE '^DAS_SCRUB_(ENABLED|TARGETS)=' <<< "$env_out")" "2"
check "6. the scrub names no label the config does not define" \
    "$(undefined_scrub_labels <<< "$env_out")" ""
# The same config without its [scrub] section: the default, the author's labels.
awk '/^\[scrub\]/ { skip = 1; next } skip && /^[[:space:]]*$/ { skip = 0 } !skip' \
    "$config" > "$work/no-scrub.toml"
check "6. without its [scrub] section it would scrub the author's three labels" \
    "$("$btrdasd" config dump-env --config "$work/no-scrub.toml" 2>&1 | undefined_scrub_labels)" \
    "primary-22tb system-recovery-A-2tb system-recovery-B-2tb"
# A scrub that is off reaches no drive, so names none it cannot find: the same
# config, its [scrub] section replaced by one that switches the scrub off.
{ cat "$work/no-scrub.toml"; printf '\n[scrub]\nenabled = false\n'; } > "$work/scrub-off.toml"
check "6. a scrub that is off is not asked to find the author's labels" \
    "$("$btrdasd" config dump-env --config "$work/scrub-off.toml" 2>&1 | undefined_scrub_labels)" ""

if ((fails > 0)); then
    echo "INSTALL DOC CONFIG SUITE RED ($fails failed)"
    exit 1
fi
echo "INSTALL DOC CONFIG SUITE GREEN"
