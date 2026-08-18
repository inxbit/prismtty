# Changelog

All notable changes to PrismTTY are documented here.

## Unreleased

### Performance

- Reuse PCRE2 match data while applying rules. Each rule ran every line
  through `captures_iter`, which allocates a match-data block and a JIT stack
  per match attempt; whole-match rules now use the regex's pooled match data
  and capture-styled rules reuse one capture block per line. Piped highlighting
  of a synthetic 20k-line router dump with four profiles drops from 0.57s to
  0.14s, and a capture-styled rule that can match empty drops from 4.3s to
  0.16s. Output is unchanged.

### Security and Reliability

- Keep the wrapped command's exit code when input reaches PrismTTY after the
  command has already exited (a paste ending in `exit`, or fast typing). The
  PTY master write then fails with EIO because the slave side is gone; that is
  the session ending, not an input failure, so PrismTTY no longer replaces the
  child's status with exit code 1 and `prismtty: I/O error`. Regression from
  1.2.0's input-worker failure reporting.
- Only terminated output lines count as remote close markers. A read boundary
  that split a benign line such as `Connection to router1 closed by
  administrator.` right after `closed` could pop the remote profile mid-session;
  the marker is now evaluated once its line is complete, and a genuine close
  marker split across reads still pops when its line ends.
- Treat the UTF-8 encoding of the C1 CSI introducer (`C2 9B`) like the raw
  `9B` and `ESC [` forms everywhere, not only in the tokenizer: alternate
  screen enter/leave, cursor and layout sequences, bracketed-paste disable,
  program SGR tracking, and prompt-echo SGR neutralization now all recognize
  it, so a program using that form no longer stays highlighted inside a
  full-screen view or loses its foreground after a highlighted token.
- Make the `--trace-io` size-limit marker terminal. Once
  `---- trace truncated at N bytes ----` is written the trace is closed at the
  limit; shorter lines no longer keep appending below the marker and the
  marker no longer repeats.
- Parse combined short options when learning the remote host from a typed
  command (`ssh -4p 2222 router1`, `ssh -vp 2222 router1`), so the close
  marker for that host still returns to the local profile instead of the
  port number being taken as the target.

### Profiles

- The generic IPv6 rule no longer paints `::` scope-resolution operators in
  compiler and stack-trace output (`MyClass::Add`, `Foo::dead`, `Ok::<u8, _>`);
  a `::`-led address must start its token, so `::1`, `[2001:db8::1]:443`, and
  `fe80::1` still highlight.

## 1.2.1 - 2026-07-11

### Release and QA

- Publish the completed 1.2.0 changes under a fresh immutable patch tag after
  the protected v1.2.0 tag stopped in validation before any release assets or
  crate were published.
- Pin `cargo-audit` to the latest Rust 1.85-compatible release in CI and
  release validation so upstream tool updates cannot break publication.

## 1.2.0 - 2026-07-11

### Library API

- Add `RuleMatchError` and fallible `try_highlight_str`,
  `try_highlight_bytes`, and `try_style_spans` APIs for callers that need to
  surface bounded PCRE2 runtime failures. The existing infallible methods keep
  their compatibility behavior by returning the original unstyled input, or
  no styled spans, when a rule cannot be evaluated.

### Security and Reliability

- Preserve the SSH destination used for dynamic-profile cleanup when option
  values contain quoted or backslash-escaped spaces or use OpenSSH's `-P` tag
  option, so the real connection close marker returns to the local profile.
- Keep bounded close-marker inspection aligned to complete output lines so a
  truncated prose line cannot be mistaken for a remote teardown marker.
- Kill signal-immune PTY descendants before reaping an exited process-group
  leader, supervise Linux-only terminating signals during raw-mode cleanup,
  and restore cooked terminal state before job-control suspension.
- Recover visible output when raw ST follows malformed UTF-8 inside an
  oversized terminal string, and cap incomplete non-string controls at 1 KiB
  so byte-fragmented CSI input cannot trigger a 16 KiB quadratic rescan.
- Run release validation and crate build verification without trusted-
  publishing OIDC authority. The minimal publishing stage uses the previously
  validated source with Cargo verification disabled, and existing GitHub
  release assets are checked before irreversible crate publication.

### Release and QA

- Test the declared Rust 1.85 minimum supported version in CI before release
  tags run the pinned release toolchain.
- Include untracked, non-ignored files in the privacy gate so new source and
  fixture files cannot bypass local pre-commit checks, and reject raw or
  conventionally compressed packet captures and trace artifacts that
  text-oriented scanning would skip.
- Move the ARM macOS release artifact to the supported macOS 15 runner image.

## 1.1.1 - 2026-07-07

### Security and Reliability

- Terminate and reap the wrapped child on failure paths with a bounded
  SIGHUP-then-SIGKILL escalation to its process group, draining the PTY master
  so a child blocked writing to the full PTY cannot wedge the exit. Previously
  a stream error while wrapping a SIGHUP-immune child hung prismtty forever,
  and an early setup error could orphan the child.
- Serve the website's fonts from the site itself instead of Google Fonts and
  ship a strict Content-Security-Policy on every page, so visitors no longer
  contact third-party hosts.

### Packaging

- Limit the crates.io package to sources, builtin profiles, license, and
  readmes (98 to 33 files).
- Extend cargo-deny unmaintained-dependency coverage to transitive
  dependencies.

### Tests

- Add a regression test that breaks the output stream mid-session against a
  SIGHUP-immune child and asserts prismtty exits and the child is reaped.
- Assert the site pages ship a CSP, reference no third-party font hosts, and
  keep the 404 page's inline-block hashes in sync.

## 1.1.0 - 2026-07-02

### Security and Reliability

- Add `--sanitize` to strip OSC, DCS, SOS, PM, APC, OSC 52 clipboard, and
  window-title string escapes from program output while preserving normal
  CSI/SGR styling.
- Handle raw 8-bit C1 string-control forms and split sanitize reads without
  leaking control payloads, while preserving UTF-8 glyphs split across chunks.
- Make `--local-echo` and `--trace-io` warnings explicit about hidden prompt
  secrets and recorded keystrokes.
- Warn once when cumulative matching time for a single rule crosses the slow
  regex threshold, pointing users at `--benchmark` instead of silently
  appearing hung.

### Profiles

- Discover symlinked `profiles.d` YAML files so dotfile-managed profile
  directories work normally while preserving existing file validation.
- Surface mutually inheriting selected profiles as cyclic inheritance errors
  instead of silently producing an empty configuration.
- Evict the oldest dynamic profile stack entry on overflow, preserving the most
  recent remote profile context.

### Tests

- Add sanitize coverage for OSC 52, DCS/SOS/PM/APC, raw C1 string controls,
  split string escapes, and split UTF-8 input under sanitize mode.
- Add regressions for symlinked profile discovery, profile stack overflow, and
  mutually inheriting selected profiles.

## 1.0.12 - 2026-06-28

### Interactive Rendering

- Surface prompt-redraw completion tails after Tab or `?` completion when the
  wrapped program redraws `prompt# command` and then goes idle, so completed
  command text no longer stays hidden until the next keystroke.
- Keep the new idle flush scoped to prompt-echo provenance, prompt-line shape,
  and a strict clean-idle PTY poll so ordinary program output and password
  prompts do not flush early.

### Reliability

- Preserve incomplete UTF-8 and ANSI escape tails while flushing buffered
  interactive echo, avoiding partial codepoint or escape emission.
- Document the prompt-echo idle-flush decision and add replay coverage for
  Cisco-style completion redraws.

## 1.0.11 - 2026-06-06

### Interactive Rendering

- Surface delimiter-less raw-mode input echo without waiting for another
  keystroke, covering ssh-style sessions where the local terminal ECHO flag is
  off but the child still returns visible echo bytes.
- Keep the echo flush scoped to idle, byte-for-byte suffix matches, so ordinary
  split program output remains buffered for cross-read highlighting unless it
  exactly matches recent type-ahead.

### Security and Reliability

- Scope recent input tracking to the current line and clear it on submit,
  interrupt, EOF, line kill, suspend, or quit controls so abandoned non-echoed
  input does not linger.
- Preserve the display boundary that only child output bytes are emitted; recent
  user input remains a bounded matching key and is never written directly.

### Tests

- Add raw-mode paste and typed-character integration coverage for ECHO-off
  child sessions, plus unit coverage for current-line recent input clearing.

## 1.0.10 - 2026-06-05

### Interactive Rendering

- Surface pasted interactive input echo once the wrapped child PTY goes idle,
  so a delimiter-less trailing token does not stay hidden until the next
  keystroke.
- Preserve cross-read highlighting for program output while type-ahead is
  present, including no-echo terminal modes such as password prompts.

### Security and Reliability

- Fail closed when an interactive PTY descriptor cannot report terminal ECHO
  state, and make the PTY descriptor duplication used for echo checks explicit
  and fallible.
- Clear recently matched echoed input after it has served the echo-tail match,
  reducing how long typed or pasted command text remains in memory.

### Tests

- Add integration coverage for pasted input visibility, split program-output
  highlighting, and concurrent no-echo input edge cases.

## 1.0.9 - 2026-05-30

### Security and Reliability

- Bound config and profile file reads to regular files under the configured
  size limit before parsing YAML.
- Count existing bytes in `--trace-io` append targets before writing new trace
  data, keeping long-running diagnostics inside the trace size cap.
- Surface panics from PTY worker threads and restore cooked terminal mode when
  the stdin forwarder fails unexpectedly.
- Make the crate Unix-only at compile time, matching its PTY, termios, and
  signal-handling runtime requirements.

### Profiles and CLI

- Warn when a `profiles.d` profile overrides a built-in profile name.
- Treat falsy reset-mode environment values such as `0`, `false`, `no`, and
  `off` as disabled.
- Narrow Fortinet and example custom-router rules so common English words such
  as `ha` and `primary` do not highlight without device-specific context.
- Anchor completion generation to the crate root so generated shell completion
  files land in the repository output directory regardless of the caller's
  current working directory.

## 1.0.8 - 2026-05-30

### Security and Reliability

- Treat oversized unterminated ANSI escape sequences as complete for
  carry-detection purposes so hostile terminal output is flushed or neutralized
  instead of being retained in streaming and interactive buffers.

### Profiles

- Highlight IPv6 addresses in the generic profile, including compressed forms
  and prefixes.
- Add Fortinet interface highlighting for common interface names such as
  `port1`, `wan2`, `mgmt`, aggregate, NPU, FortiLink, and SSL VPN interfaces.
- Narrow Versa object highlighting to distinctive object tokens so ordinary
  prose such as `tenant`, `branch`, and `controller` is not highlighted as a
  Versa object.

### Tests

- Add focused regression coverage for oversized incomplete escapes, IPv6
  highlighting, Fortinet interfaces, Versa object false positives, and unstyled
  replay tokens.

## 1.0.7 - 2026-05-30

### Security and Reliability

- Bound `--strip-ansi` carry buffering for unterminated ANSI escape sequences
  so hostile remote output cannot grow memory without limit.
- Preserve split ANSI escape stripping across reads without leaking parameter
  bytes into visible output.
- Keep streaming output valid when multibyte UTF-8 codepoints split across read
  boundaries.
- Reject empty and whitespace-only native profile names before profile
  registration.
- Narrow dynamic profile close-marker detection to teardown-shaped lines so
  benign log prose does not pop an active remote profile.

### Performance

- Cache compiled highlighters across dynamic profile switches with a bounded
  cache, avoiding repeated regex compilation during profile flapping.

## 1.0.6 - 2026-05-30

### Security and Reliability

- Keep highlighted UTF-8 output valid when byte-mode PCRE2 matches land inside
  multibyte characters.
- Reject unknown top-level, profile, and rule fields in user YAML so misspelled
  configuration fails closed instead of being silently ignored.
- Forward external termination signals to the wrapped child process group and
  preserve signal-derived child exit statuses.
- Clamp child exit codes above 255 instead of truncating them to a misleading
  success code.

### Profiles

- Restore generic syslog severity highlighting for realistic `%FACILITY-N-MSG`
  tags.
- Narrow Cisco, Juniper, ArubaCX, Linux/Unix, and Versa profile rules so common
  prose and unhealthy states are not highlighted as healthy interface or status
  output.

### Release

- Add a clippy `-D warnings` gate to CI.
- Add regression coverage for PTY signal forwarding, exit-status mapping,
  UTF-8-safe highlighting, strict YAML parsing, and profile false-positive
  fixes.

## 1.0.5 - 2026-05-27

### Interactive Rendering

- Reset active interactive highlighting when a stream finishes so subsequent
  terminal output is not left in PrismTTY's foreground color.
- Honor full-reset mode while neutralizing source SGR sequences in Fortinet
  prompt echo, instead of always using minimal foreground resets.

### CLI

- Preserve `--` command delimiters when wrapping commands named `profiles`, so
  `ptty -- profiles ...` runs the command instead of invoking PrismTTY's
  internal profile subcommand.

### Release

- Map macOS `arm64` hosts to the published `darwin-aarch64` release target when
  building local release archives without an explicit target argument.

## 1.0.4 - 2026-05-27

### Interactive Rendering

- Flush buffered typed-character echo after keystroke-sized interactive reads
  so colon and menu prompts surface input immediately without weakening
  token buffering for replay or noninteractive streams.
- Preserve incomplete trailing escape sequences during the interactive echo
  flush so partial CSI controls are not emitted as raw terminal bytes.

### Release Security

- Require tag-triggered releases to prove the tag commit is on `main`, verify
  downloaded artifact checksums before attestation/publish, and include
  `Cargo.lock` in privacy scans for sensitive local markers.

### Tests

- Add regression coverage for colon-prompt typed echo, noninteractive token
  buffering, partial escape preservation, and nested-session iTerm guard
  environment assertions.

## 1.0.3 - 2026-05-25

### Configuration

- Include the offending file path when file-loaded ChromaTerm config or
  PrismTTY profile YAML cannot be parsed.
- Reject invalid named capture style keys during config parsing, while keeping
  numeric capture indexes supported.

### Reliability

- Cover PTY size fallback to standard `24x80` dimensions when terminal size
  detection does not return usable rows and columns.

### Performance

- Reuse prompt-echo line buffers while scanning interactive chunks instead of
  allocating a temporary vector for each visible line.

## 1.0.2 - 2026-05-24

### Maintenance

- Stabilize release and parser-path tests by isolating `PRISMTTY_NO_MINIMAL_RESET`
  and `PRISMTTY_NO_39_49_RESETS` environment state during argument parsing
  checks.
- Add deterministic environment restore helpers with explicit lock ordering for
  test execution safety.

## 1.0.1 - 2026-05-23

### Interactive Rendering

- Add `--no-minimal-reset` plus `PRISMTTY_NO_MINIMAL_RESET` and
  `PRISMTTY_NO_39_49_RESETS` for terminal emulators that ignore minimal
  foreground/background reset codes in interactive streams.
- Reset active PrismTTY styling before Cisco-style help redraws so completed
  words do not leak highlight color into the prompt or command tail.
- Preserve Cisco command tails across split help, cursor-positioning, and prompt
  redraw chunks.
- Preserve interactive prompt state when dynamic profile rebuilds replace the
  active highlighter.

### Tests

- Add Cisco redraw regression coverage for command-tail preservation and
  configurable full-reset behavior.
- Add Fortinet replay coverage for merged fixture output.

## 1.0.0 - 2026-05-20

### Security and Reliability

- Harden release publishing by rerunning the privacy scan in the publish job
  and rejecting unsupported package targets before release paths are built.
- Bound dynamic profile state and detection buffers so repeated remote output
  cannot grow profile switching state without limit.
- Move raw-mode signal cleanup out of signal-handler context and keep PTY
  resize handling tied to terminal resize notifications.
- Harden malformed ANSI and local RGB style parsing so invalid input is
  neutralized or rejected without leaking styling state or panicking.
- Make runtime PID registration, reload bookkeeping, and legacy runtime layout
  compatibility more robust.

### Performance

- Reuse loaded profile stores, indexed benchmark lookups, prepared stream
  chunks, and static xterm-256 gray steps to reduce repeated allocation and
  parsing work.

### Profiles and Highlighting

- Keep child profile rules ahead of inherited parent rules, skip profile
  directories named with YAML extensions, and preserve interactive
  non-foreground attributes such as underline and background colors.
- Improve prompt and close-marker detection across split terminal chunks,
  macOS/zsh prompts, and local echo escape sequences.

## 0.2.7 - 2026-05-19

### Internal Reliability

- Make the dynamic-profile input queue regression test exercise the actual
  full-queue path while keeping the receiver alive, so the bounded observation
  behavior stays covered without changing stdin forwarding.
- Avoid per-byte temporary `String` allocations when formatting `--trace-io`
  hex diagnostics.

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
