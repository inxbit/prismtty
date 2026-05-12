# Security Policy

## Supported Versions

Security fixes are handled on the latest release and the current `main` branch.
Older releases may receive fixes when the affected code is still relevant and a
safe backport is practical.

## Reporting a Vulnerability

Please do not open a public issue with exploit details, private terminal
captures, credentials, or sensitive environment information.

Use GitHub private vulnerability reporting for this repository when available.
If private reporting is not available, open a minimal public issue asking for a
security contact and omit technical details until a private channel is arranged.

Useful reports include:

- Affected PrismTTY version or commit.
- Operating system and terminal environment.
- The smallest synthetic input or config that reproduces the issue.
- Expected and observed behavior.
- Impact assessment, especially whether the input crosses a trust boundary such
  as remote terminal output, local config parsing, release packaging, or trace
  file creation.

## Sensitive Data

PrismTTY is often used around network devices and administrative shells. Do not
attach real device captures, private hostnames, customer names, IP inventories,
credentials, or trace files from sensitive sessions. Reduce reports to synthetic
examples that preserve only the token shape needed to reproduce the issue.

