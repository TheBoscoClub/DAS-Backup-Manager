#!/bin/bash
# A running backup script survives a rewrite of its own file
# (bd DAS-Backup-Manager-6wt, fix round 4).
#
# Bash does not load a script before running it: after each command it reads
# the next one from the file, at the offset where the last one ended. A copy
# over the installed script in place — truncate, then write, the same inode,
# as a plain `cp` does and `btrdasd setup` did — that landed while a run was
# inside main() let main finish and then had bash read whatever the NEW file
# held at that offset: a tail, half a line (exit 127), a stranger's commands.
# A last line of
#
#     main "$@"; exit $?
#
# is read whole before main runs, and once main returns bash has nothing left
# to read: it exits with main's status.
#
# The experiment: a scratch script, `set -euo pipefail` like the real ones,
# blocks inside main until told to go; the file is rewritten in place — the
# same file longer, a different and longer script, a shorter one — then main
# finishes. The exit status must be main's and nothing but main may run. The
# control runs the same experiment with the old last line, `main "$@"`, and
# must show the defect, or the experiment shows nothing. Last, each of the
# three installed scripts must end with exactly that line: the experiment
# proves the mechanism, the assertion proves the scripts use it.
#
# Writes only beneath a mktemp directory. No root, no devices, no network.

# The single-quoted lines below are a script's text, written out literally:
# their $1, $2, $@ and $? belong to that script, not to this one.
# shellcheck disable=SC2016

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(dirname "$here")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

FINAL='main "$@"; exit $?'
OLD='main "$@"'

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

# write_script <file> <last line>: main signals it has started, waits for
# "$1/go", prints MAIN-DONE and returns "$2".
write_script() {
    printf '%s\n' \
        '#!/bin/bash' \
        'set -euo pipefail' \
        'main() {' \
        '    : >"$1/ready"' \
        '    while [[ ! -e "$1/go" ]]; do sleep 0.01; done' \
        '    echo MAIN-DONE' \
        '    return "$2"' \
        '}' \
        "$2" >"$1"
}

# run_rewritten <last line> <main's status> <longer|different|shorter|atomic>
# Prints "<exit status>|<output, one line>".
run_rewritten() {
    local last="$1" status="$2" kind="$3"
    local dir script pid rc inode waited=0
    dir="$(mktemp -d "$work/run.XXXXXX")"
    script="$dir/script.sh"
    write_script "$script" "$last"
    inode="$(stat -c %i "$script")"
    bash "$script" "$dir" "$status" >"$dir/out" 2>&1 &
    pid=$!
    while [[ ! -e "$dir/ready" ]]; do
        sleep 0.01
        waited=$((waited + 1))
        if ((waited > 1000)); then
            kill "$pid" 2>/dev/null
            echo "main never started"
            return
        fi
    done
    case "$kind" in
    longer) {
        cat "$script"
        printf 'echo TAIL-RAN\nexit 99\n'
    } >"$dir/new" ;;
    different)
        for i in $(seq 1 200); do echo "echo DIFFERENT-LINE-$i-xxxxxxxxxxxxxxxx"; done >"$dir/new"
        ;;
    shorter | atomic) printf '#!/bin/bash\necho SHORT\n' >"$dir/new" ;;
    esac
    if [[ "$kind" == atomic ]]; then
        # A new file renamed over the name, as setup writes since this round.
        mv "$dir/new" "$script"
        [[ "$(stat -c %i "$script")" != "$inode" ]] || echo "the atomic rewrite kept the inode"
    else
        # In place: the same inode truncated and written, as a plain cp copies.
        cat "$dir/new" >"$script"
        [[ "$(stat -c %i "$script")" == "$inode" ]] || echo "the rewrite was not in place"
    fi
    : >"$dir/go"
    wait "$pid"
    rc=$?
    echo "$rc|$(tr '\n' ' ' <"$dir/out")"
}

echo "== with the last line '$FINAL', a rewrite in place during main changes nothing"
for status in 0 7; do
    for kind in longer different shorter; do
        check "main returns $status, file rewritten $kind" \
            "$(run_rewritten "$FINAL" "$status" "$kind")" "$status|MAIN-DONE "
    done
done

echo "== control: with the old last line '$OLD' the same rewrite runs the new file"
got="$(run_rewritten "$OLD" 0 longer)"
check "old ending, rewritten longer: the new tail runs" "$got" "99|MAIN-DONE TAIL-RAN "
# Which of its bytes bash lands on decides what runs — whole lines, or half
# of one (exit 127) — so this asserts only that something besides main did.
got="$(run_rewritten "$OLD" 0 different)"
if [[ "$got" != "0|MAIN-DONE " ]]; then
    echo "ok   old ending, rewritten as another script: more than main ran ($got)"
else
    echo "FAIL old ending, rewritten as another script: only main ran"
    fails=$((fails + 1))
fi
check "old ending, file replaced atomically: the old file runs to its end" \
    "$(run_rewritten "$OLD" 0 atomic)" "0|MAIN-DONE "

echo "== the three installed scripts end with exactly '$FINAL'"
for name in backup-run.sh backup-verify.sh boot-archive-cleanup.sh; do
    file="$repo/scripts/$name"
    check "$name: last line" "$(tail -n 1 "$file")" "$FINAL"
    check "$name: ends with a newline" "$(tail -c 1 "$file" | od -An -c | tr -d ' ')" '\n'
done

if ((fails > 0)); then
    echo "SCRIPT REWRITE SUITE RED ($fails failed)"
    exit 1
fi
echo "SCRIPT REWRITE SUITE GREEN"
