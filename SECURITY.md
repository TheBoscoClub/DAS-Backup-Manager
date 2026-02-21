# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| latest  | :white_check_mark: |

## Reporting a Vulnerability

If you discover a security vulnerability in this project, please report it responsibly:

1. **Do NOT open a public GitHub issue** for security vulnerabilities
2. Use GitHub's [private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability) feature
3. Or email the maintainer directly
4. Include: description, steps to reproduce, potential impact, suggested fix (if available)

## GitHub Security Features

| Feature | Status | Purpose |
|---------|--------|---------|
| **Dependabot vulnerability alerts** | Enabled | Monitor dependencies for known vulnerabilities |
| **Dependabot security updates** | Enabled | Auto-create PRs to fix vulnerable dependencies |
| **Secret scanning** | Enabled | Detect accidentally committed secrets |
| **CodeQL analysis** | Enabled | Deep semantic code analysis |

## Security Audit Tools

### C++ Code

| Tool | Purpose | Command |
|------|---------|---------|
| **clang-tidy** | Static analysis and modernization | `clang-tidy src/*.cpp -- -std=c++20` |
| **cppcheck** | Static analysis for C/C++ | `cppcheck --enable=all src/` |

### Shell Scripts

| Tool | Purpose | Command |
|------|---------|---------|
| **shfmt** | Shell formatting validation | `shfmt -d scripts/*.sh` |
| **shellcheck** | Shell script static analysis | `shellcheck scripts/*.sh` |

### General

| Tool | Purpose | Command |
|------|---------|---------|
| **codespell** | Spell checking for typos | `codespell --skip='.git,build'` |
| **markdownlint** | Markdown linting | `markdownlint '**/*.md'` |
| **trivy** | Filesystem vulnerability scanner | `trivy fs .` |

## Security Alert Resolution Policy

| Alert Type | Resolution Approach |
|------------|---------------------|
| **Critical/High CVE** | Fix immediately or create tracking issue |
| **Medium CVE** | Fix within 30 days |
| **Low CVE** | Fix in next release cycle |
| **False Positive** | Dismiss with documented reason |
| **Won't Fix** | Dismiss with documented justification |

## Contact

For security concerns, use GitHub's private vulnerability reporting or contact the maintainer.
