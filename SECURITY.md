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
- Whether the terminal reads UTF-8 is inferred once, at startup, from the
  text of the locale variables: the first of `LC_ALL`, `LC_CTYPE` and `LANG`
  that is set and not empty must name the UTF-8 codeset (`en_US.UTF-8`,
  `C.utf8`, or `UTF-8` alone). PrismTTY does not check that the locale exists
  or that the terminal really reads UTF-8: `bogus.UTF-8` counts, and a
  nonempty `LC_ALL=C` in front of a UTF-8 `LANG` does not.
- In a UTF-8 locale an isolated invalid raw byte in ordinary text, a byte
  from 80 to 9F that is not part of a UTF-8 character, is text in every mode,
  as it is on a terminal that reads UTF-8: it opens and ends no control, and
  `--sanitize` and `--strip-ansi` keep it and the ordinary text after it. A
  byte from that range inside an ESC, CSI or string control that is already
  open belongs to that control and may be removed with it, and the usual
  buffering of a trailing token or of a character cut off at the end of a
  read still applies. In any other locale, or with none set, such a byte is
  the control an 8-bit terminal reads: a `90`, `98`, `9D`, `9E` or `9F` opens
  a string that runs to its terminator, and everything after an unterminated
  one is withheld.
- A terminal that reads 8-bit bytes behind a UTF-8 locale runs such a byte as
  a C1 control. There a raw `9D` opens an OSC string, an OSC 52 clipboard
  write included, that both `--sanitize` and `--strip-ansi` let through, and
  a raw `9B` opens a CSI. Keep the locale and the terminal in step.
- The answer does not change during a run. A program that takes the terminal
  out of UTF-8 with `ESC % @` or DEC private mode 42 and then sends raw C1
  bytes is still read as above: PrismTTY takes no color from a raw `9B`
  sequence and follows no screen switch in it, so on a terminal that honors
  the switch a color can be lost after a highlight, a switch to or from the
  alternate screen missed, and text the terminal holds in a raw string
  highlighted. `--sanitize` removes both switches, so the terminal stays in
  UTF-8. Without a UTF-8 locale a raw `9B` is read as the control an 8-bit
  terminal runs, and a color it sets is written back in the 7-bit form after
  a highlight. In a UTF-8 locale no color is taken from the UTF-8 form
  `C2 9B` either, because terminals that read UTF-8 do not agree on running
  it; on one that does, a color set that way is not restored after a
  highlight.
- By default PrismTTY reads an ESC followed by a byte from 80 up as an ESC
  that ends before that byte. A terminal like xterm ignores an invalid byte
  there and still holds the ESC open, and output PrismTTY adds can then
  finish it. Two cases are known; 1.4.1 reaches both when the byte is FF, and
  in a UTF-8 locale a raw byte from 80 to 9F now reaches them too. A
  highlight in the text after `ESC`, such a byte and `]` ends the OSC string
  the terminal opened there, so `A ESC 9D ]52;c;QUJD error` without a
  terminator reaches it as an OSC 52 that ends at the highlight. An
  unfinished ESC at the end of the output is written as `^`, so
  `log ESC 9E ESC` ends as `log ESC 9E ^`, which such a terminal reads as the
  start of a PM string that can hide what is written after PrismTTY ends.
  `--sanitize` drops an ESC in front of a byte from 80 up, and so both.
- By default PrismTTY passes the escape sequences of the wrapped program
  through as they are, within limits. A string control (OSC, DCS, SOS, PM or
  APC) that is still open after 16 KiB is dropped from its introducer through
  its terminator, even when it does end later, so a large sixel or inline
  image does not reach the terminal. When working against untrusted remote
  devices, `--sanitize` strips string-type
  escapes (window title, OSC 52 clipboard, DCS/SOS/PM/APC payloads) from
  program output while leaving colors and cursor control intact. That
  includes the title string of GNU screen and tmux, `ESC k title ESC \`,
  which only those two read as a string. If an ESC is followed by C0 controls
  and one of PrismTTY's reads ends after 1 KiB or more of them, before the
  byte that would introduce a string, `--sanitize` and `--strip-ansi` drop the
  ESC and show the introducer and the payload as text; with the ESC gone, the
  terminal runs no string control.
- `--sanitize` and `--strip-ansi` guard against string controls (OSC, DCS,
  SOS, PM, APC) for a terminal that reads UTF-8 in step with the locale. They
  do not remove every
  terminal control: `--sanitize` keeps CSI sequences such as colors and
  screen clears on purpose, as above, and both keep C0 controls such as BEL
  and line ends. In plain text, a UTF-8 character that is not itself a C1
  control passes both flags in any locale, even when one of its bytes is
  `90`, `98`, `9D`, `9E` or `9F`, and a terminal that reads 8-bit bytes
  instead reads that byte as a control: the last byte of U+201D (`E2 80 9D`)
  is `9D`, the start of an OSC string there. A character cut off at the end
  of the output (`F0 9D`) is passed as it is too. So on a terminal that reads
  8-bit bytes, whatever the locale, these flags do not keep every string
  control from reaching it. When a filter processes the bytes it drops a `C2`
  that begins no character, so a removal cannot join a `C2` and a raw byte
  into a C1 control; a `C2` cut off by the end of the output is written as it
  is, and the next byte another program writes can complete it.
- The filters work on the bytes of one PrismTTY process and promise nothing
  about what another process writes to the same terminal before or after it.
  On a terminal, `--sanitize` does not neutralize every incomplete control
  at the end of input: an unfinished ESC can reach the terminal as it came,
  as after `user@host $ 9D X` in a UTF-8 locale, and whatever writes to the
  terminal next, such as the shell's next prompt, can finish it, as an OSC 52
  among others.
- Live reload uses `prismtty-<uid>` in the system temporary directory, a name
  other local users can predict. When that directory is one others can write
  to, such as `/tmp` on Linux without `TMPDIR` or a shared directory named by
  `TMPDIR`, a user who creates the name first disables live reload for your
  sessions. The sessions' output is not affected, and each prints
  `prismtty: live reload disabled for this session` and the reason. Set
  `PRISMTTY_RUNTIME_DIR` or `TMPDIR` to a private directory to avoid it. The
  default temporary directory on macOS is per user, also with `TMPDIR` unset,
  and is not affected.
