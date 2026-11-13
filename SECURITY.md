# Security Policy

## Supported Versions

| Version | Supported |
| --- | --- |
| latest | :white_check_mark: |

## Reporting a Vulnerability

If you discover a security vulnerability in this project, please report it responsibly:

1. **Do NOT open a public GitHub issue** for security vulnerabilities
2. Use GitHub's [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability) feature
3. Or email the maintainer directly
4. Include: description, steps to reproduce, potential impact, suggested fix (if available)

## GitHub Security Features

| Feature | Status | Purpose |
| --- | --- | --- |
| **Dependabot vulnerability alerts** | Enabled | Monitor dependencies for known vulnerabilities |
| **Dependabot security updates** | Enabled | Auto-create PRs to fix vulnerable dependencies |
| **Secret scanning** | Enabled | Detect accidentally committed secrets |
| **CodeQL analysis** | Enabled | Deep semantic code analysis |

## Security Audit Tools

### Rust Code

| Tool | Purpose | Command |
| --- | --- | --- |
| **clippy** | Lints, warnings as errors (CI) | `cargo clippy --all-targets --features dbus -- -D warnings` |
| **cargo-mutants** | Mutation testing gates every push (`.github/workflows/mutants.yml`) | see `indexer/.cargo/mutants.toml` |
| **semgrep** | Static analysis in CI (`.github/workflows/semgrep.yml`) | `semgrep ci` |
| **cargo-deny** | Advisories (RustSec), yanked crates, licences, registries and git sources, over every feature (CI job `deny`; policy `indexer/deny.toml`) | `cd indexer && cargo deny check advisories bans licenses sources` |

### C++ Code

| Tool | Purpose | Command |
| --- | --- | --- |
| **clang-tidy** | Static analysis and modernization | `clang-tidy src/*.cpp -- -std=c++20` |
| **cppcheck** | Static analysis for C/C++ | `cppcheck --enable=all src/` |

### Shell Scripts

| Tool | Purpose | Command |
| --- | --- | --- |
| **shfmt** | Shell formatting validation | `shfmt -d scripts/*.sh` |
| **shellcheck** | Shell script static analysis | `shellcheck scripts/*.sh` |

### General

| Tool | Purpose | Command |
| --- | --- | --- |
| **codespell** | Spell checking for typos | `codespell --skip='.git,build'` |
| **markdownlint** | Markdown linting | `markdownlint '**/*.md'` |
| **yamllint** | Workflow and config YAML, strict (CI job `yaml`; settings `.yamllint`) | `yamllint --strict .github/` |
| **trivy** | Filesystem vulnerability scanner | `trivy fs .` |

### Continuous Integration

`.github/workflows/ci.yml` runs these jobs on every push to `main` and every pull request: `rust` (fmt, clippy, test), `loopback`, `build` (Rust library, CLI and Qt6 GUI), `shell` (shellcheck), `deny` (cargo-deny), `spelling` (codespell), `yaml` (yamllint) and `markdown` (markdownlint).

| Control | Purpose | Where |
| --- | --- | --- |
| **SHA-pinned actions** | Every `uses:` in `ci.yml`, `mutants.yml` and `release-packages.yml` names a full commit SHA, with its version in a trailing comment, so a moved tag cannot change what runs | `.github/workflows/` |
| **Checksummed rustup installer** | The release workflow's `.deb` and AppImage jobs download a pinned `rustup-init` and run it only after `sha256sum -c` accepts the digest committed in the workflow; nothing is piped from `curl` into `sh` | `.github/workflows/release-packages.yml` |
| **Root loopback suites** | Scrub, subvolume sync and boot archival run as root against real BTRFS filesystems on loop devices (the `loopback` job); a loop device left behind fails the job | `.github/scripts/run-root-loopback-suites.sh` |

## Security Alert Resolution Policy

| Alert Type | Resolution Approach |
| --- | --- |
| **Critical/High CVE** | Fix immediately or create tracking issue |
| **Medium CVE** | Fix within 30 days |
| **Low CVE** | Fix in next release cycle |
| **False Positive** | Dismiss with documented reason |
| **Won't Fix** | Dismiss with documented justification |

## Contact

For security concerns, use GitHub's private vulnerability reporting or contact the maintainer.
