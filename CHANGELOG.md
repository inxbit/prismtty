# Changelog

All notable changes to PrismTTY are documented here.

## 0.2.2 - 2026-05-13

### Interactive Rendering

- Recognise zsh's promptless line-edit redraws (cursor positioning combined
  with a non-prompt line tail) and re-arm prompt-echo passthrough so
  single-byte typed echoes after a completion menu are not held in the
  streaming splitter's pending buffer until a space arrives.
- Retire prompt-echo passthrough explicitly on bracketed-paste-disable
  (`CSI ?2004l`) emitted by zsh before each command runs, instead of relying
  on the `\r`/`\n` reset path.
- Add regression coverage for typed echoes after completion redraws, promptless
  redraws, cursor-only repaints, and recovery of ordinary highlighting after
  cursor-positioning progress output without a recognised prompt marker.

### Diagnostics

- `--trace-io` now prefixes every line with the monotonic elapsed time
  (`SECS.USECS`) since the trace was opened, so IN→OUT and OUT→RENDER gaps can
  be measured directly. Parsers that consumed the previous
  `DIRECTION HEX...` format need to skip the leading timestamp column.

## 0.2.1 - 2026-05-12

### Security and Reliability

- Reject cyclic profile inheritance with a structured config error instead of
  recursing until stack overflow.
- Add CLI regression coverage for self-inheriting and mutually inheriting
  profiles.
- Replace deprecated `serde_yaml` runtime parsing with `serde_norway`.
- Pin GitHub Actions in CI and release workflows to full-length commit SHAs.
- Scope the release workflow token so only the publishing job has
  `contents: write`.

### Packaging

- Generate the Homebrew formula from release checksum files instead of keeping a
  stale formula with placeholder SHA-256 values.
- Stop advertising unsupported Linux ARM Homebrew artifacts until the release
  workflow builds them.
- Avoid requiring `ripgrep` on GitHub-hosted runners for CI formula and privacy
  checks.
