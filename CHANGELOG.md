# Changelog

All notable changes to PrismTTY are documented here.

## 0.2.6 - 2026-05-16

### Security and Reliability

- Add a `cargo-deny` policy and enforce advisory, license, bans, and source
  checks alongside `cargo audit` in CI and the release workflow.
- Harden `--trace-io` on Unix by rejecting symlink targets, non-regular files,
  non-current-user files, and existing trace files without owner-only
  permissions.
- Bound dynamic-profile stdin observation with a fixed-size queue and
  drop-on-full behavior so silent remote sessions cannot accumulate unbounded
  profile-observation input.
- Make the PCRE2 JIT stack limit explicit at the upstream default until the
  Rust `pcre2` wrapper exposes match/depth limit setters.
- Expand the privacy scan to catch Linux home-directory captures, private-ish
  hostnames, and non-documentation IPv4 addresses while preserving synthetic
  fixture ranges.

### Release

- Reorder the release workflow so artifacts are downloaded, validated,
  Homebrew formula generation succeeds, and release artifacts are attested
  before publishing to crates.io.
- Write release checksums with the published asset filename so validation,
  GitHub release assets, and Homebrew formula generation use the same names.
- Keep downloaded release artifacts outside the Git checkout so crates.io
  publishing runs from a clean working tree.
- Add release artifact provenance through GitHub artifact attestations.
- Replace misleading pinned-toolchain workflow comments with stable-toolchain
  wording.

### Documentation

- Refresh the GitHub, crates.io, and website copy around stdin usage, installed
  commands, built-in profiles, runtime reload, and social sharing metadata.

## 0.2.5 - 2026-05-15

### Documentation

- Add Rustdoc examples for `Highlighter::from_config`,
  `Highlighter::highlight_str`, `StreamingHighlighter::push_str`, and
  `config::parse_profile_yaml` so docs.rs shows concrete public API usage.
- Add a docs.rs badge to the GitHub and crates.io README badge rows.
- Rework the "What This Is / What This Is Not" README section into skimmable
  lists for community-review traffic.

## 0.2.4 - 2026-05-14

### Documentation

- Add an animated terminal demo to the GitHub README and website.
- Clarify what PrismTTY is and is not across the README, crates.io README, and
  website.
- Add feedback-wanted guidance for built-in vendor profiles and custom profile
  files.
- Add public rustdoc coverage for the crate API so docs.rs presents documented
  modules, types, fields, variants, and functions.

### Website

- Update the homepage preview, scope, and feedback sections for promotion.

## 0.2.3 - 2026-05-14

### Security and Reliability

- Keep runtime registration and reload marker files under a private
  user-owned runtime directory, tighten runtime file permissions on Unix, and
  reject non-file runtime entries or symlink targets before reading or writing
  them.
- Write `--trace-io` diagnostics with owner-only permissions on Unix so trace
  files created in shared locations are not left world-readable.
- Bound streaming output buffering for unterminated ANSI, OSC, and DCS escape
  sequences by neutralizing oversized incomplete controls before they can hold
  later output indefinitely.

### Tests

- Add regression coverage for private runtime paths, trace file permissions,
  runtime symlink rejection, oversized unterminated escape buffering, and split
  terminated OSC preservation.

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
