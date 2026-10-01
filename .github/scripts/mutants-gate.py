#!/usr/bin/env python3
"""Fail a cargo-mutants run that verified nothing.

WHY THIS EXISTS
    `cargo mutants` exits 0 when every mutant was CAUGHT and also when every
    mutant was UNVIABLE (failed to compile). Those are opposite outcomes with
    the same exit code, so a gate named "falsifiability" can silently stop
    falsifying anything and keep reporting green.

    Observed 2026-08-27 (cachyos-sentinel, run 33104946077): "6 mutants tested
    in 3m: 6 unviable" plus "WARN No mutants were viable" — job green. The
    changed functions returned a domain enum and a Result alias, and
    cargo-mutants' primary mutation is "replace the body with a default
    return"; neither type can be built that way, so every candidate failed to
    compile. The gate is weakest on exactly the rich return types most worth
    mutating.

WHAT IT ENFORCES
    0. The run was not corrupted before scoring (CARGO_TARGET_DIR unset)
    1. No mutant survived        (missed == 0 and timeout == 0)
    2. Something was actually tested   (viable > 0 whenever mutants existed)
    3. The check itself ran      (a missing/!unreadable outcomes.json is a
                                  FAILURE, never a pass — a check that cannot
                                  report failure is not a check)

RULE 0, added 2026-09-01 (cachyos-sentinel m4wn)
    cargo-mutants copies the tree to its own scratch directory PER MUTANT and
    builds there. Forcing CARGO_TARGET_DIR sends every one of those parallel
    builds to ONE shared target dir, where they overwrite each other and get
    scored against the wrong binaries.

    Measured on identical source, same command, only the environment differing:
        CARGO_TARGET_DIR set    ->  10 / 11 / 6 / 3 missed across four runs
        CARGO_TARGET_DIR unset  ->  2 missed, stable across three runs
    Three mutants the corrupted runs called MISSED were proven CAUGHT by hand.

    It corrupts in BOTH directions, and the dangerous one is a real coverage
    hole reported as caught — a gate reporting success it has not earned, which
    is the exact class this script exists to prevent.

    There is no trade-off to weigh: cargo-mutants ALREADY builds under /tmp, so
    the house build-to-RAM rule is satisfied without the variable. Setting it
    buys nothing and costs correctness. Run `env -u CARGO_TARGET_DIR cargo
    mutants ...` in any shell where the build-to-RAM export is active.

USAGE
    mutants-gate.py [--outcomes PATH] [--label NAME] [--allow-zero-total]

    --outcomes         path to outcomes.json (default: mutants.out/outcomes.json)
    --label            printed in the summary line; use the crate/scope name
    --allow-zero-total tolerate a run that generated NO mutants at all. Only
                       for scopes where an empty mutant set is legitimate.
                       It does NOT relax rule 2: if mutants existed, some must
                       have been viable.

EXIT
    0  gate passed
    1  gate failed (reason on stderr)

Dependency-free. Python 3.8+.
"""
import argparse
import json
import os
import sys


def emit(line: str) -> None:
    """Print, and append to the GitHub job summary when running in Actions."""
    print(line)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        try:
            with open(summary, "a", encoding="utf-8") as fh:
                fh.write(line + "\n")
        except OSError:
            pass  # a summary we cannot write must not mask the gate's verdict


def fail(msg: str) -> "int":
    print(f"MUTANTS GATE: FAIL — {msg}", file=sys.stderr)
    return 1


def main() -> int:
    ap = argparse.ArgumentParser(add_help=True)
    ap.add_argument("--outcomes", default="mutants.out/outcomes.json")
    ap.add_argument("--label", default="")
    ap.add_argument("--allow-zero-total", action="store_true")
    args = ap.parse_args()

    tag = f"[{args.label}] " if args.label else ""

    # Rule 0 — refuse to SCORE a run that may have been corrupted. This is
    # checked before the outcomes are even read, because if the environment
    # was wrong the numbers in that file are not evidence of anything.
    #
    # Deliberately no escape hatch: a flag to override would be used exactly
    # when someone is in a hurry, which is when the corruption matters most.
    # Re-run the gate with the variable unset instead.
    shared_target = os.environ.get("CARGO_TARGET_DIR")
    if shared_target:
        return fail(
            f"{tag}CARGO_TARGET_DIR is set ({shared_target!r}). cargo-mutants "
            "builds each mutant in its own scratch tree; forcing one shared "
            "target dir makes those parallel builds race and score against "
            "each other's binaries, silently — in BOTH directions. Refusing "
            "to score this run. cargo-mutants already builds under /tmp, so "
            "the variable buys nothing here: re-run as "
            "`env -u CARGO_TARGET_DIR cargo mutants ...` and re-run this gate "
            "the same way."
        )

    # Rule 3 — fail closed. If the run did not produce outcomes, we cannot
    # know what happened, and "cannot know" is not "fine".
    try:
        with open(args.outcomes, encoding="utf-8") as fh:
            d = json.load(fh)
    except FileNotFoundError:
        return fail(
            f"{tag}{args.outcomes} does not exist. cargo-mutants did not run, "
            "died before writing outcomes, or ran in another directory. "
            "Not treating an absent result as success."
        )
    except (OSError, json.JSONDecodeError) as e:
        return fail(f"{tag}{args.outcomes} is unreadable ({e}).")

    try:
        total = int(d["total_mutants"])
        caught = int(d["caught"])
        missed = int(d["missed"])
        timeout = int(d["timeout"])
        unviable = int(d["unviable"])
    except (KeyError, TypeError, ValueError) as e:
        return fail(
            f"{tag}outcomes.json is missing an expected counter ({e}). "
            "The cargo-mutants output schema may have changed — fix this "
            "script rather than dropping the gate."
        )

    viable = caught + missed + timeout
    emit(
        f"mutants {tag}total={total} caught={caught} missed={missed} "
        f"timeout={timeout} unviable={unviable} viable={viable}"
    )

    # Rule 1 — a surviving mutant is a test that cannot fail.
    if missed or timeout:
        return fail(
            f"{tag}{missed} missed and {timeout} timed-out mutant(s) survived. "
            "Each one is a change to production code that no test detects."
        )

    # Rule 2 — the vacuity check this script exists for.
    if total == 0:
        if args.allow_zero_total:
            emit(f"mutants {tag}no mutants generated for this scope — allowed")
            return 0
        return fail(
            f"{tag}0 mutants were generated. Nothing was tested. If the scope "
            "legitimately has no mutable code, pass --allow-zero-total; if it "
            "does not, the file/glob selection is wrong. NOTE: cargo-mutants "
            "--file takes a GLOB, not a directory — 'src/policy/' matches "
            "nothing and yields 0 mutants, while 'src/policy/**/*.rs' works."
        )
    if viable == 0:
        return fail(
            f"{tag}{total} mutant(s) generated but NONE was viable — every one "
            "failed to compile, so this run verified nothing while exiting 0. "
            "Usual cause: the mutated functions return types cargo-mutants "
            "cannot construct (a domain enum, a Result alias), so its "
            "replace-the-body mutation never compiles. Widen the scope, or "
            "cover these functions in the scheduled full-module run."
        )

    emit(f"mutants {tag}OK — {viable} viable mutant(s), all caught")
    return 0


if __name__ == "__main__":
    sys.exit(main())
