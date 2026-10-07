#!/bin/bash
# parent_disk in scripts/backup-run.sh (bd DAS-Backup-Manager-b1s): the disk
# under a partition is read from lsblk, never made by stripping trailing
# digits (that turns /dev/nvme0n1p2 into /dev/nvme0n1p, which is no device),
# and when lsblk cannot say the answer is "unknown" -- nothing printed,
# status 1 -- never a guessed name.
#
# The real function is extracted from the script and run against a stub lsblk
# on PATH. Writes only beneath a mktemp directory. No root, no devices.
#
# SC2016: the stub lsblk is single-quoted on purpose.
# shellcheck disable=SC2016

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="${PARENT_DISK_SCRIPT:-$ROOT/scripts/backup-run.sh}"
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

sed -n '/^parent_disk() {/,/^}/p' "$SCRIPT" >"$WORK/fn.sh"
[[ -s "$WORK/fn.sh" ]] || {
    echo "HARNESS BROKEN: parent_disk not found in $SCRIPT"
    exit 2
}
grep -q lsblk "$WORK/fn.sh" || {
    echo "HARNESS BROKEN: parent_disk no longer asks lsblk"
    exit 2
}

# A stub lsblk answering from a table: STUB_TYPE / STUB_PKNAME, or failing.
mkdir "$WORK/bin"
cat >"$WORK/bin/lsblk" <<'EOF'
#!/bin/bash
[[ -z "${STUB_FAIL:-}" ]] || exit 32
case "$*" in
    *TYPE*) printf '%s\n' "${STUB_TYPE-}" ;;
    *PKNAME*) printf '%s\n' "${STUB_PKNAME-}" ;;
esac
EOF
chmod +x "$WORK/bin/lsblk"

run() { # run <device>: prints "<stdout>|<status>"
    local out st
    out="$(
        # shellcheck source=/dev/null
        source "$WORK/fn.sh"
        PATH="$WORK/bin:$PATH" parent_disk "$1"
    )"
    st=$?
    echo "$out|$st"
}

check "nvme partition -> its disk, not nvme0n1p" \
    "$(STUB_TYPE=part STUB_PKNAME=nvme0n1 run /dev/nvme0n1p2)" "/dev/nvme0n1|0"
check "sata partition -> its disk" \
    "$(STUB_TYPE=part STUB_PKNAME=sde run /dev/sde1)" "/dev/sde|0"
check "a whole disk is its own disk" \
    "$(STUB_TYPE=disk run /dev/sdb)" "/dev/sdb|0"
check "lsblk failing is unknown, not a guess" \
    "$(STUB_FAIL=1 run /dev/sde1)" "|1"
check "a partition with no parent name is unknown" \
    "$(STUB_TYPE=part STUB_PKNAME='' run /dev/sde1)" "|1"
check "a device lsblk does not know is unknown" \
    "$(STUB_TYPE='' run /dev/nope)" "|1"
check "a device mapper node is not guessed at" \
    "$(STUB_TYPE=crypt run /dev/mapper/x)" "|1"

echo "$pass passed, $fail failed"
[[ $fail -eq 0 ]]
