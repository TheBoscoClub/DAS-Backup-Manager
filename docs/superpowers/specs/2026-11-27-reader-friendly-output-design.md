# Reader-friendly error, stop and diagnostic output — design

Short design, operator 2026-11-27. **Status, plan and decisions: bd `DAS-Backup-Manager-3gzs.12`** (the
recovery-OS session log: bd `DAS-Backup-Manager-eg4j.19` Task 8).

## Intent

On 2026-11-27 the operator missed the real cause of a refused recovery-OS update because it was one line among
unformatted ones, next to a 3000-character list of unit names. His words: errors, stops and diagnostics "don't
need to be more verbose ... they just need to be formatted differently for quicker visual acquisition of relevant
data". Applies to any error, stop or diagnostic output: terminal, journal, GUI, and the emailed backup report.

## Rules (every output this touches)

1. **Nothing added, nothing lost.** A shortened item (a long list → a count) stays in full where it is recorded
   (JSON record, history, database), and the short form says where.
2. **The deciding line stands apart.** What made the job refuse, fail or stop is its own line or block, set
   apart from routine lines (a blank line or a header before it; its status word first: `REFUSED`, `FAILED`,
   `WARN`, `STOPPED`).
3. **A RESULT block at the end** of every multi-item job: one row per item (drive, target, filesystem, check),
   status word in a fixed column, one-clause reason. An item not run is a row too, with why.
4. **Aligned label columns** for key/value blocks (`label` padded, value after); one fact per line; each value
   once (no path repeated in the same line).
5. **Words, not raw values**: `present, started by udev …`, not `read true true`; booleans and codes become what
   they mean, the code kept in brackets where a reader may need it.
6. **Local time with zone** (`2026-11-27 03:19 CST`) where a time is shown to the operator; the age beside it.
7. **Headers**: `━━ <job> ━━` for a run, `── <item> ──` for each item; plain ASCII fallback is not needed (UTF-8
   everywhere this runs). GUI: the same structure as rich text or a table, not a monospace dump.
8. **Machine-read lines are not broken.** Any line another program parses (`CLEAR TARGET:`, `RAID GUARD:`,
   `RESULT <label> <exit> <outcome>`, report rows tests match) keeps its exact form, or every consumer changes with
   it in the same commit, found by grep, never by memory.
9. **Decisions do not change.** Only how output is printed; exit codes, statuses and what is recorded stay.

## Approaches

1. Per-output reformat under shared rules (chosen): each output is small and owned by one file.
2. A shared formatting library across Rust, bash and C++: three languages, little shared code; rejected for now
   (a Rust helper module is fine where several Rust outputs share it).

## Out of scope

- More verbosity, new data, new checks.
