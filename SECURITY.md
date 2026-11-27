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
- PrismTTY is not a descriptor isolation boundary. Since 1.3.15, descriptors
  above 2 that the caller leaves inheritable, whether passed on purpose
  (`ptty cmd 3<file`) or leaked by a terminal, IDE, multiplexer or launcher,
  reach the wrapped command as they would without the wrapper. Descriptors
  marked close-on-exec do not. On a system where the PTY's name cannot be
  found (a chroot without /proc), inheritable descriptors may still be closed
  for a command started from a terminal. A launcher that holds sensitive
  descriptors should close them or set close-on-exec before it starts
  PrismTTY.
- User-supplied rule regexes run against every byte of terminal output.
  PrismTTY reads output in 8 KiB chunks, but chunking alone is not a per-match
  execution budget. Every compiled rule also receives an application-owned
  PCRE2 match limit of 100,000 and depth limit of 1,000. Limit and other runtime
  match errors are counted in rule telemetry and surfaced once per session.
  These limits are defense in depth, not proof that every accepted pattern is
  cheap: expensive patterns (for example unanchored lookaheads such as
  `(?=.*foo)`) can still consume substantial CPU on long lines. PrismTTY also
  warns once per session when a rule crosses five seconds of cumulative
  matching time; use `--benchmark` to identify slow rules in a config.
- A raw byte `90`, `98`, `9D`, `9E` or `9F` that is not part of a UTF-8
  character opens a string control only on a terminal that is not in UTF-8
  mode. PrismTTY reads it that way unless the locale says UTF-8 (the first of
  `LC_ALL`, `LC_CTYPE` and `LANG` that is set). In a UTF-8 locale such a
  string ends at the end of its line, so a stray byte from line noise or a
  legacy code page cannot hide the rest of the session; `--sanitize` still
  removes the byte and the rest of that line. In any other locale, or with
  none set, the string runs to its terminator and everything after an
  unterminated one is withheld.
- Whether the terminal reads UTF-8 is decided once, from the locale at
  startup. In a UTF-8 locale PrismTTY treats a control sequence opened by a
  raw byte `9B` as text for the whole run: it takes no color from it and
  follows no screen switch in it, because a UTF-8 terminal does not run it.
  A program that takes the terminal out of UTF-8 with `ESC % @` or DEC
  private mode 42 and then uses raw `9B` sequences can therefore lose its
  color after a highlight, and have its switch to or from the alternate
  screen missed, on a terminal that honors that switch. `--sanitize` removes
  both switches, so the terminal stays in UTF-8. Without a UTF-8 locale a raw
  `9B` is read as the control an 8-bit terminal runs, and a color it sets is
  written back in the 7-bit form after a highlight.
- By default PrismTTY passes the escape sequences of the wrapped program
  through as they are, within limits. A string control (OSC, DCS, SOS, PM or
  APC) that is still open after 16 KiB is dropped from its introducer through
  its terminator, even when it does end later, so a large sixel or inline
  image does not reach the terminal. In a UTF-8 locale the same limit applies
  to the stray raw byte above: when the byte and the rest of its line come to
  16 KiB or more, both are dropped and only the line end is kept. When
  working against untrusted remote devices, `--sanitize` strips string-type
  escapes (window title, OSC 52 clipboard, DCS/SOS/PM/APC payloads) from
  program output while leaving colors and cursor control intact. That
  includes the title string of GNU screen and tmux, `ESC k title ESC \`,
  which only those two read as a string. If an ESC is followed by C0 controls
  and one of PrismTTY's reads ends after 1 KiB or more of them, before the
  byte that would introduce a string, `--sanitize` and `--strip-ansi` drop the
  ESC and show the introducer and the payload as text; with the ESC gone, the
  terminal runs no string control.
- `--sanitize` and `--strip-ansi` protect a terminal that reads UTF-8. Both
  keep every valid UTF-8 character, in any locale, and a terminal that reads
  8-bit bytes instead can find a control in one: the last byte of U+201D
  (`E2 80 9D`) is `9D`, which such a terminal reads as the start of an OSC
  string. A character cut off at the end of the output (`F0 9D`) is passed as
  it is too. With a locale that is not UTF-8 and an 8-bit terminal, these
  flags do not keep every string control from reaching it.
- On Linux without `TMPDIR`, live reload uses `/tmp/prismtty-<uid>`, a name
  other local users can predict. One who creates it first disables live
  reload for your sessions. The sessions' output is not affected, and each
  prints `prismtty: live reload disabled for this session` and the reason.
  Set `PRISMTTY_RUNTIME_DIR` to a private directory to avoid it.
