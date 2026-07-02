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

## Operational Cautions

- `--local-echo` echoes every typed printable key, including secrets typed at
  prompts that deliberately suppress echo (password prompts). Leave it off when
  entering credentials.
- `--trace-io` records all PTY input and output to the trace file, including
  typed passwords. The file is created owner-only (0600), but treat it as
  sensitive and delete it after debugging.
- User-supplied rule regexes run against every byte of terminal output.
  PrismTTY bounds each match (PCRE2 match limits, 8 KiB read chunks), so a
  pathological pattern cannot hang it, but expensive patterns (for example
  unanchored lookaheads such as `(?=.*foo)`) can slow throughput severely on
  very long lines. PrismTTY warns once per session when a rule crosses five
  seconds of cumulative matching time; use `--benchmark` to identify slow
  rules in a config.
- By default PrismTTY is a transparent wrapper: escape sequences from the
  wrapped program pass through unchanged, exactly like `ssh` or `tmux`. When
  working against untrusted remote devices, `--sanitize` strips string-type
  escapes (window title, OSC 52 clipboard, DCS/SOS/PM/APC payloads) from
  program output while leaving colors and cursor control intact.

