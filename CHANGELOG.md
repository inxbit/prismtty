# Changelog

All notable changes to PrismTTY are documented here.

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
