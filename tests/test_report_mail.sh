#!/bin/bash
#
# tests/test_report_mail.sh
#
# The shipped direction of the mail guard (bd DAS-Backup-Manager-1x76). Every
# test build refuses to spawn mailx (report::MAILX, feature test-overrides),
# and every Rust test proves only that refusal. This suite runs the btrdasd a
# build makes (REAL_BTRDASD; ctest sets it to the CMake-built release binary)
# through a real Rust report send and checks that it reaches mailx, with
# exactly the arguments and body the relay is to get. A btrdasd built with
# test-overrides (by --all-features, a default change, or a feature that
# implies it) mails nothing and keeps its exit code; here it fails.
#
# The send: `btrdasd doctor --email`. The doctor runs unprivileged, so it
# cannot open /run/das-doctor.lock, and mails its "could not run" report —
# no lock is taken, nothing is mounted. Run as root (CI's container), the
# suite runs btrdasd as nobody, so a host's real /run locks are never taken.
#
# mailx is a stub first on an `env -i` PATH: it keeps its arguments and its
# stdin and answers as told. It runs no mailer and never its own name. The
# config names an unreachable relay (127.0.0.1:1) and no source or target.
# Writes only beneath a mktemp directory. No network, no devices.
#
# The D-Bus helper (REAL_BTRDASD_HELPER; ctest sets it when the helper is
# built) mails the GUI's backup reports through the same report::MAILX, but
# its send needs root, the system bus, polkit and a real backup job, so it is
# not driven here. Instead both shipped binaries are searched for the
# refusal's text: a release build without test-overrides has none (the
# guarded branch is a constant false, so the text is never compiled in), and
# one with the feature — `dbus` implying it, say — has it and fails here.
set -uo pipefail
# The task cap (tests/lib/bound.sh): a runaway PATH stub stops here, not at oomd
# shellcheck source=/dev/null
source "$(dirname "${BASH_SOURCE[0]}")/lib/bound.sh"

WORK="$(mktemp -d)" && [[ -d "$WORK" ]] || {
    echo "HARNESS BROKEN: no temp dir"
    exit 2
}
trap 'rm -rf "${WORK:?}"' EXIT

pass=0
fail=0
check() { # check <name> <got> <want>
    if [[ "$2" == "$3" ]]; then
        echo "ok   $1"
        pass=$((pass + 1))
    else
        echo "FAIL $1 -- got '$2', want '$3'"
        fail=$((fail + 1))
    fi
}
has() { # has <name> <text> <needle>
    if grep -qF -- "$3" <<<"$2"; then
        echo "ok   $1"
        pass=$((pass + 1))
    else
        echo "FAIL $1 -- no '$3' in: $2"
        fail=$((fail + 1))
    fi
}

if [[ -z "${REAL_BTRDASD:-}" ]]; then
    # Visible, never a pass: CI fails a suite that prints this.
    echo "NOT RUN: set REAL_BTRDASD to a built btrdasd (ctest sets it to the binary CMake built)"
    exit 0
fi
[[ -x "$REAL_BTRDASD" ]] || {
    echo "HARNESS BROKEN: REAL_BTRDASD=$REAL_BTRDASD is not executable"
    exit 2
}

# Run as: ourselves, or nobody when we are root.
AS=()
if ((EUID == 0)); then
    SETPRIV="$(type -P setpriv)" || {
        echo "NOT RUN: running as root and no setpriv to drop to nobody with"
        exit 0
    }
    AS=("$SETPRIV" --reuid=nobody --regid=nobody --clear-groups --)
fi

STUBS="$WORK/stubs"
REC="$WORK/rec"
mkdir -p "$STUBS" "$REC"
cp -- "$REAL_BTRDASD" "$WORK/btrdasd"
cat >"$STUBS/mailx" <<'EOF'
#!/bin/bash
# Keeps its arguments (one per line) and its stdin; answers as $MAILX_RECORD/rc says.
printf '%s\n' "$@" >"$MAILX_RECORD/args"
cat >"$MAILX_RECORD/body"
rc="$(cat "$MAILX_RECORD/rc" 2>/dev/null || echo 0)"
[[ "$rc" == 0 ]] || echo "smtp-server: 127.0.0.1:1: Connection refused (stub)" >&2
exit "$rc"
EOF
chmod 755 "$WORK" "$STUBS" "$STUBS/mailx" "$WORK/btrdasd"
chmod 777 "$REC"

config() { # config <enabled>
    cat >"$WORK/config.toml" <<EOF
[general]
version = "0.7.23"
install_prefix = "/usr"
db_path = "$WORK/index.db"
btrbk_conf = "$WORK/btrbk.conf"
[init]
system = "systemd"
[schedule]
incremental = "03:00"
full = "Sun 04:00"
randomized_delay_min = 30
[email]
enabled = $1
to = "ops@example.org"
from = "das@example.org"
smtp_host = "127.0.0.1"
smtp_port = 1
[gui]
enabled = false
EOF
    chmod 644 "$WORK/config.toml"
}

RC=0
doctor() { # doctor [VAR=value...]: btrdasd doctor --email, output to $WORK/out
    rm -f "${REC:?}/args" "${REC:?}/body"
    RC=0
    env -i PATH="$STUBS:/usr/bin:/bin" HOME="$WORK" LC_ALL=C MAILX_RECORD="$REC" "$@" \
        "${AS[@]}" "$WORK/btrdasd" doctor --email --config "$WORK/config.toml" \
        >"$WORK/out" 2>&1 || RC=$?
}
# How many times the refusal's text is in a file (grep -c counts lines).
REFUSAL="a test build never mails"
refusals() { grep -caF -- "$REFUSAL" "$1" || true; }
ran() { if [[ -f "$REC/args" ]]; then echo yes; else echo no; fi; }

# The PATH the run gets resolves mailx to the stub.
check "the run's mailx is the stub" \
    "$(env -i PATH="$STUBS:/usr/bin:/bin" bash -c 'type -P mailx')" "$STUBS/mailx"

host="$(cat /proc/sys/kernel/hostname)"
sep="═══════════════════════════════════════════════════════════"

echo "== email enabled, the relay takes it"
config true
echo 0 >"$REC/rc"
doctor
out="$(cat "$WORK/out")"
check "exit status: could not run" "$RC" "2"
check "mailx was run" "$(ran)" "yes"
args=()
[[ -f "$REC/args" ]] && mapfile -t args <"$REC/args"
check "mailx got 13 arguments" "${#args[@]}" "13"
check "arg 1" "${args[0]:-}" "-s"
# [[:digit:]], never a range: under en_US.UTF-8 [0-9] matches non-ASCII digits (bd 1bsx).
subject_re='^\[DAS Doctor\] (.+) — FAILURE — [[:digit:]]{4}-[[:digit:]]{2}-[[:digit:]]{2} [[:digit:]]{2}:[[:digit:]]{2}$'
if [[ "${args[1]:-}" =~ $subject_re ]]; then
    check "subject: the kernel's host name, FAILURE, the minute" "${BASH_REMATCH[1]}" "$host"
else
    check "subject: [DAS Doctor] <host> — FAILURE — <YYYY-MM-DD HH:MM>" "${args[1]:-}" "[DAS Doctor] $host — FAILURE — <minute>"
fi
check "arg 3" "${args[2]:-}" "-r"
check "From: display name and the configured sender" "${args[3]:-}" "DAS Doctor (${host%%.*}) <das@example.org>"
check "arg 5" "${args[4]:-}" "-S"
check "v15-compat" "${args[5]:-}" "v15-compat"
check "arg 7" "${args[6]:-}" "-S"
check "the relay" "${args[7]:-}" "mta=smtp://127.0.0.1:1"
check "arg 9" "${args[8]:-}" "-S"
check "no authentication" "${args[9]:-}" "smtp-auth=none"
check "arg 11" "${args[10]:-}" "-S"
check "no dead.letter" "${args[11]:-}" "nosave"
check "the recipient" "${args[12]:-}" "ops@example.org"
why="$(grep -m1 '^Error: ' <<<"$out")"
why="${why#Error: }"
check "the doctor said why it could not run" "$([[ -n "$why" ]] && echo yes || echo no)" "yes"
want_body="$(printf '%s\n  DAS Subvolume Drift Report\n  Status: COULD NOT RUN — FAILURE\n%s\n\nThe drift check could not run: %s\n' "$sep" "$sep" "$why")"
check "the body is the could-not-run report, on stdin" "$(cat "$REC/body" 2>/dev/null)" "$want_body"
has "it says the report went" "$out" "Doctor failure report emailed"

echo "== email enabled, the relay refuses"
echo 1 >"$REC/rc"
doctor
out="$(cat "$WORK/out")"
check "exit status unchanged" "$RC" "2"
check "mailx was run" "$(ran)" "yes"
has "mailx's stderr reaches the warning" "$out" \
    "Could not send doctor failure report email: mailx failed (exit status: 1): smtp-server: 127.0.0.1:1: Connection refused (stub)"

echo "== DAS_REPORT_TO and DAS_REPORT_FROM replace the configured recipient and sender"
echo 0 >"$REC/rc"
doctor DAS_REPORT_TO=probe-to@example.org DAS_REPORT_FROM=probe-from@example.org
args=()
[[ -f "$REC/args" ]] && mapfile -t args <"$REC/args"
check "mailx got 13 arguments" "${#args[@]}" "13"
check "the recipient is DAS_REPORT_TO" "${args[12]:-}" "probe-to@example.org"
check "the sender is DAS_REPORT_FROM, given the display name" "${args[3]:-}" "DAS Doctor (${host%%.*}) <probe-from@example.org>"
doctor "DAS_REPORT_FROM=Probe <probe-from@example.org>"
args=()
[[ -f "$REC/args" ]] && mapfile -t args <"$REC/args"
check "a sender with its own display name is kept verbatim" "${args[3]:-}" "Probe <probe-from@example.org>"
check "without DAS_REPORT_TO, the configured recipient" "${args[12]:-}" "ops@example.org"

echo "== email disabled"
config false
echo 0 >"$REC/rc"
doctor
out="$(cat "$WORK/out")"
check "exit status" "$RC" "2"
check "mailx was not run" "$(ran)" "no"
has "it says why not" "$out" "Could not send doctor failure report email: Email is not enabled in config"

echo "== neither shipped binary carries the test-build refusal"
printf 'x %s y\n' "$REFUSAL" >"$WORK/control"
check "the search finds the text where it is (control)" "$(refusals "$WORK/control")" "1"
check "btrdasd: no refusal" "$(refusals "$REAL_BTRDASD")" "0"
if [[ -z "${REAL_BTRDASD_HELPER:-}" ]]; then
    # Visible, never a pass: CI fails a suite that prints this.
    echo "NOT RUN: the D-Bus helper's check — set REAL_BTRDASD_HELPER to a built btrdasd-helper"
elif [[ ! -f "$REAL_BTRDASD_HELPER" ]]; then
    check "btrdasd-helper exists" "no such file: $REAL_BTRDASD_HELPER" "a built btrdasd-helper"
else
    check "btrdasd-helper: no refusal" "$(refusals "$REAL_BTRDASD_HELPER")" "0"
fi

echo "passed=$pass failed=$fail"
if ((fail > 0)); then
    echo "REPORT MAIL SUITE RED"
    exit 1
fi
echo "REPORT MAIL SUITE GREEN"
