#!/bin/bash
# A DESTDIR install of a finished build puts every file under DESTDIR — the
# shell completions among them — and runs nothing on the installing machine
# (bd DAS-Backup-Manager-cway).
#
# CMakeLists.txt used to run, from install(CODE):
#   - `systemctl daemon-reload` and `systemctl enable btrdasd-helper.service`,
#     on whatever machine ran the install: under DESTDIR, the package build host;
#   - `${CMAKE_INSTALL_PREFIX}/bin/btrdasd completions <shell>`, written under
#     ${CMAKE_INSTALL_PREFIX}/share: DESTDIR ignored, so a package build ran
#     the build host's own btrdasd (an old one, or none) and wrote outside the
#     package, which shipped no completions;
# every one with ERROR_QUIET and no RESULT_VARIABLE, so nothing ever failed.
#
# Usage: test_install_destdir.sh <cmake> <build-dir> --completions|--no-completions
# Registered with ctest as shell-install-destdir; it needs the finished build.
#
# The build is installed with DESTDIR a fresh scratch directory and a stub
# `systemctl` first on PATH that records every call. Checks:
#   1. the install succeeds;
#   2. systemctl is never called;
#   3. the install manifest and the files under DESTDIR are the same list, so
#      every installed file is under DESTDIR and none was written there unlisted;
#   4. no generated install script runs a command (execute_process) — install
#      time may check and print, never act. The one exception is CMake's own
#      `cmake --install --strip` support, which it writes under
#      `if(CMAKE_INSTALL_DO_STRIP)` and which strips the copy under DESTDIR;
#   5. --completions: the bash, zsh and fish completions are installed and are
#      byte for byte what this build's btrdasd prints; --no-completions: none.
# Out of its sight: install-time code that writes outside DESTDIR without
# running a command (a file(WRITE) to an absolute path) — neither under DESTDIR
# nor in the manifest. Configuring with a scratch prefix exposes that too:
# nothing may then appear under the prefix itself.
#
# Writes beneath a mktemp directory, and the build tree's install_manifest.txt
# (which every install rewrites). No root, no network.

set -uo pipefail

if (($# != 3)) || [[ "$3" != --completions && "$3" != --no-completions ]]; then
    echo "usage: $0 <cmake> <build-dir> --completions|--no-completions" >&2
    exit 2
fi
cmake="$1"
build="$2"
expect_completions="$3"

if [[ ! -f "$build/cmake_install.cmake" ]]; then
    echo "FAIL: $build is not a configured build tree (no cmake_install.cmake)" >&2
    exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
dest="$work/destdir"
calls="$work/systemctl.calls"
manifest="$build/install_manifest.txt"

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

mkdir -p "$work/bin"
cat > "$work/bin/systemctl" <<EOF
#!/bin/bash
printf '%s\n' "\$*" >> "$calls"
EOF
chmod +x "$work/bin/systemctl"

# Without this the systemctl check could not fail: it would watch a stub the
# install never reaches.
check "the stub is the systemctl PATH finds" \
    "$(PATH="$work/bin:$PATH" command -v systemctl)" "$work/bin/systemctl"

# The manifest read below must be the one this install writes.
rm -f "$manifest"
out="$(PATH="$work/bin:$PATH" DESTDIR="$dest" "$cmake" --install "$build" 2>&1)"
rc=$?
check "1. the install succeeds" "$rc" "0"
((rc == 0)) || printf '%s\n' "$out" | tail -n 20

check "2. systemctl is never called" \
    "$(cat "$calls" 2>/dev/null | paste -sd '|' -)" ""

if [[ -s "$manifest" ]]; then
    # CMake writes each entry as the file's final path, DESTDIR stripped, so
    # the files under DESTDIR are compared with DESTDIR stripped too. An entry
    # with no file under DESTDIR, or a file there that no install() rule
    # wrote, is a line of this diff.
    stray="$(diff <(sort "$manifest") \
                  <(find "$dest" \( -type f -o -type l \) -print \
                    | awk -v d="$dest" 'index($0, d) == 1 { print substr($0, length(d) + 1) }' \
                    | sort) \
             | grep '^[<>]' | paste -sd '|' -)"
    check "3. the manifest is exactly the files under DESTDIR" "$stray" ""
else
    check "3. the install wrote a manifest" "missing or empty" "present"
fi

mapfile -t install_scripts < <(find "$build" -name cmake_install.cmake -print | sort)
check "4. install scripts found" "$((${#install_scripts[@]} > 0))" "1"
acts="$(awk '
    FNR == 1 { prev = "" }
    /execute_process\(/ && prev !~ /^[[:space:]]*if\(CMAKE_INSTALL_DO_STRIP\)[[:space:]]*$/ {
        print FILENAME ":" FNR
    }
    NF { prev = $0 }
' "${install_scripts[@]}" | paste -sd ' ' -)"
check "4. no install script runs a command" "$acts" ""

# The manifest's entry for one shell's completion: the line that ends with
# /share/<path>.
installed() {
    awk -v s="/share/$1" \
        'length($0) >= length(s) && substr($0, length($0) - length(s) + 1) == s' \
        "$manifest" 2>/dev/null
}

btrdasd="$build/cargo-target/release/btrdasd"
declare -A path_of=(
    [bash]="bash-completion/completions/btrdasd"
    [zsh]="zsh/site-functions/_btrdasd"
    [fish]="fish/vendor_completions.d/btrdasd.fish"
)
for shell in bash zsh fish; do
    entry="$(installed "${path_of[$shell]}")"
    if [[ "$expect_completions" == --no-completions ]]; then
        check "5. no $shell completion without the indexer" "$entry" ""
        continue
    fi
    check "5. $shell completion installed once" \
        "$(printf '%s' "$entry" | grep -c .)" "1"
    # The entry is the final path; the file this install wrote is under DESTDIR.
    file="$dest$entry"
    check "5. $shell completion is under DESTDIR and not empty" \
        "$([[ -n "$entry" && -s "$file" ]] && echo yes || echo no)" "yes"
    [[ -n "$entry" && -s "$file" ]] || continue
    if [[ -x "$btrdasd" ]]; then
        check "5. $shell completion is what this build's btrdasd prints" \
            "$(cmp -s "$file" <("$btrdasd" completions "$shell") && echo same || echo differs)" \
            "same"
    else
        check "5. this build's btrdasd exists, to compare against" "missing: $btrdasd" "present"
    fi
done

if ((fails > 0)); then
    echo "INSTALL DESTDIR SUITE RED ($fails failed)"
    exit 1
fi
echo "INSTALL DESTDIR SUITE GREEN"
