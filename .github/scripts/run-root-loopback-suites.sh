#!/bin/bash
# Run every root loopback integration suite (indexer/tests/*_loopback.rs) and
# fail unless each one really ran all of its tests (bd DAS-Backup-Manager-2kb1.8).
#
# These suites are #[ignore]d: they create loop devices, device-mapper targets
# and real BTRFS mounts, so a plain `cargo test` skips them, and they went
# unrun, and red, for weeks (5 of 11 by 2026-11-08). The binaries are built as
# the unprivileged user and only the test binary runs as root, so the build
# tree never turns root-owned.
#
# A green result needs, per suite: the binary exits 0; exactly one
# `test result:` line; passed equal to the number of #[ignore] attributes in the
# source file (a count that follows the file, not a number kept here); and
# failed, ignored and filtered out all 0. A suite that selects or runs nothing
# therefore fails instead of passing.
#
# Environment:
#   ROOT_RUNNER   words that put a command under root (default: sudo -E)
#   LOG_DIR       where each suite's output goes (default: a new temp directory)
#   ONLY_SUITES   space-separated suite names to run (hand runs only; CI runs all)
#   SUITE_TIMEOUT seconds one suite may run (default: 1200)
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../indexer"

read -ra root_runner <<<"${ROOT_RUNNER:-sudo -E}"
log_dir=${LOG_DIR:-$(mktemp -d)}
mkdir -p "$log_dir"
suite_timeout=${SUITE_TIMEOUT:-1200}

shopt -s nullglob
sources=(tests/*_loopback.rs)
if [ ${#sources[@]} -eq 0 ]; then
  echo "::error::no tests/*_loopback.rs found: failing rather than silently passing"
  exit 1
fi

failed=0
for src in "${sources[@]}"; do
  suite=$(basename "$src" .rs)
  # Hand runs only: CI never sets ONLY_SUITES. A developer machine whose backup
  # holds the real /run/das-maintenance.lock cannot run the scrub suite.
  if [ -n "${ONLY_SUITES:-}" ] && [[ " $ONLY_SUITES " != *" $suite "* ]]; then
    echo "$suite: skipped (ONLY_SUITES=$ONLY_SUITES)"
    continue
  fi
  expected=$(grep -c '^#\[ignore' "$src" || true)
  if [ "$expected" -eq 0 ]; then
    echo "::error::$suite: no #[ignore] test in $src: nothing would run"
    failed=1
    continue
  fi

  echo "::group::$suite: build"
  cargo test --test "$suite" --no-run --message-format=json >"$log_dir/$suite.build.json"
  bin=$(jq -r --arg s "$suite" \
    'select(.reason == "compiler-artifact" and .target.name == $s and .executable != null) | .executable' \
    "$log_dir/$suite.build.json")
  echo "::endgroup::"
  if [ -z "$bin" ] || [ "$(wc -l <<<"$bin")" -ne 1 ] || [ ! -x "$bin" ]; then
    echo "::error::$suite: cargo named no single test executable (got: '$bin')"
    failed=1
    continue
  fi

  echo "::group::$suite: run as root ($expected tests expected)"
  rc=0
  timeout "$suite_timeout" "${root_runner[@]}" "$bin" --ignored --nocapture --test-threads=1 \
    >"$log_dir/$suite.log" 2>&1 || rc=$?
  cat "$log_dir/$suite.log"
  echo "::endgroup::"

  result=$(grep -E '^test result:' "$log_dir/$suite.log" || true)
  re='^test result: ok\. ([[:digit:]]+) passed; ([[:digit:]]+) failed; ([[:digit:]]+) ignored; [[:digit:]]+ measured; ([[:digit:]]+) filtered out'
  if [ "$rc" -ne 0 ]; then
    echo "::error::$suite: exited $rc (124 is the ${suite_timeout}s timeout): ${result:-no result line}"
    failed=1
  elif [ "$(wc -l <<<"$result")" -ne 1 ] || [[ ! $result =~ $re ]]; then
    echo "::error::$suite: expected one 'test result: ok.' line, got: '${result:-nothing}'"
    failed=1
  elif [ "${BASH_REMATCH[1]}" -ne "$expected" ] || [ "${BASH_REMATCH[2]}" -ne 0 ] ||
    [ "${BASH_REMATCH[3]}" -ne 0 ] || [ "${BASH_REMATCH[4]}" -ne 0 ]; then
    echo "::error::$suite: $expected tests expected, got: $result"
    failed=1
  elif grep -q 'skipping: not root' "$log_dir/$suite.log"; then
    # boot_archive_loopback returns early (a pass) when it is not root.
    echo "::error::$suite: a test skipped itself as not root, so it proved nothing"
    failed=1
  else
    echo "$suite: OK, $expected of $expected passed"
  fi
done

[ "$failed" -eq 0 ]
