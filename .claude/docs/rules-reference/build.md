<!-- markdownlint-disable-file MD025 -- verbatim excerpts moved from .claude/rules/; reformatting would break the lossless-move proof -->
# Build Conventions — reference (NOT auto-loaded)

Holds the detail behind `.claude/rules/build.md`, same headings.

# Build Conventions

<!-- full original lines moved from the rule file, 2026-11-01 -->
Installation specifics (host toolchain versions): `.claude/rules/local-installation.md` (gitignored, this machine only).

## Single Canonical Source — Project-Specific Paths

<!-- full original lines moved from the rule file, 2026-11-01 -->
- **Binaries**: cmake installs to `/usr/bin/` and `/usr/libexec/`; symlinks only if other paths need them. The prefix defaults to `/usr` at the top of `CMakeLists.txt` and every `DESTINATION` is prefix-relative (bd `laj`); never build an absolute destination from `${CMAKE_INSTALL_PREFIX}` at configure time — the GUI's `KDEInstallDirs` resets the cached prefix later.

## Rust (buttered_dasd library + btrdasd CLI)

<!-- full original lines moved from the rule file, 2026-11-01 -->
- `buttered_dasd` exports 23 public modules (`adopt`, `backup`, `btrbk_conf`, `caldate`, `config`, `db`, `doctor`, `expire`, `forget`, `fsutil`, `health`, `indexer`, `maintenance`, `mount`, `progress`, `reconcile`, `recovery_os`, `report`, `restore`, `scanner`, `schedule`, `scrub`, `subvol`); `setup/` is binary-only. Verify with `grep -c '^pub mod ' indexer/src/lib.rs`, don't trust this line
- **Mutation testing gates every push** (`.github/workflows/mutants.yml`, scored by `.github/scripts/mutants-gate.py`) plus a weekly full scope (files listed in the workflow). New code that spawns a command or prompts must go through a test-drivable seam (`CommandRunner`, `Prompter`, the `*_with` cores), or its mutants survive. Local run: header of `indexer/.cargo/mutants.toml` — `--in-place`, crate-relative diff, on a copy of the tree, never `-- --lib`

## SQLite

<!-- full original lines moved from the rule file, 2026-11-01 -->
- SQLite 3.53.2, bundled by rusqlite (libsqlite3-sys 0.38.2), with FTS5
