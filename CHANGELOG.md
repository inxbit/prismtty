# Changelog

All notable changes to PrismTTY are documented here.

## 1.5.0 - unreleased

### Changed

- SIGUSR1 and SIGUSR2 sent to PrismTTY no longer end it with 128 plus the
  signal. PrismTTY passes them to the command it started, alone, while that
  command runs, as `kill -USR1` of the command would without PrismTTY, and
  the session goes on with the terminal in raw mode. Signals of one kind can
  coalesce, and one that arrives after PrismTTY began to end the session is
  not passed on. A signal that PrismTTY inherited as ignored or blocked, as
  `nohup` leaves SIGHUP and a script's background job leaves SIGINT and
  SIGQUIT, is still not passed on and does not end PrismTTY, for SIGUSR1 and
  SIGUSR2 as for the signals that end it.

  A worker that handles SIGUSR1 while the command PrismTTY started ignores it
  gets nothing, and a shell that leaves SIGUSR1 at its default action dies
  from it and ends the session with 128 plus the signal. To reach a server
  started through a shell, start it with `exec`
  (`prismtty -- sh -c 'exec server'`) or send the signal to the server's own
  pid.

- When PrismTTY is sent SIGTERM, SIGHUP, SIGINT, SIGQUIT or another signal
  that ends it, and the command exits by itself, PrismTTY now draws the output
  that reached it and exits with the command's exit code instead of 128 plus
  the signal. A SIGTERM handler that exits 0 gives 0 and one that exits 7
  gives 7, and a shell that reports its worker's death by SIGKILL as 137 gives
  137. A supervisor that reads 143 as stopped now sees the command's code.

  The teardown is the one of 1.4.0: while the command runs, the signal goes to
  its process group, and what is left of the group is killed as soon as the
  command exits, or after two seconds. The command has exited by itself when
  its wait status is an ordinary exit, even one that came near or after the
  two seconds. The code describes the command PrismTTY started, not its
  process group: 0 does not mean that every process in the group finished
  cleanly.

  The end stays as abrupt as in 1.4.0, with 128 plus the signal, when the
  command was killed by a signal or could not be collected, or when the normal
  end cannot complete within about four seconds of the signal, as when nothing
  reads PrismTTY's output; output not yet drawn can then be lost. The four
  seconds count while PrismTTY is not stopped; on macOS a PrismTTY that leads
  its terminal's session also waits, as it exits, while output it wrote to
  that terminal is still unread. When PrismTTY was sent SIGINT and the command
  died from it, PrismTTY still ends by SIGINT, and a command that dies from
  SIGINT after PrismTTY was sent SIGTERM still gives 143. PrismTTY acts on the
  first ending signal it receives while its handlers are installed, also while
  the session is ending; a later one does not replace it, and from then on
  PrismTTY keeps the signals caught until it exits, catching them again at
  once if it was giving them back. A session that no ending signal ended gives
  the signals back to their previous actions as it ends, and one that arrives
  after that takes that action, as before.

  This changes two statements of the 1.4.0 entry below: a signal other than
  SIGINT sent to PrismTTY no longer always ends it with 128 plus that signal,
  and a command that handles a SIGINT PrismTTY passed on keeps its exit code
  instead of 130.

## 1.4.1 - 2026-11-27

### Fixed

- In interactive output, an open highlight no longer colors the blank line a
  line feed scrolls in, or the cells a carriage return goes back over, where
  it would show: when its style paints blank cells, or when the program's own
  inverse, strike-through, overline or underline without a color of its own
  draws blank cells in the foreground color. The highlight now closes right
  before the CR or LF:
  - For a CR or LF in the text under such a program rendition, it opens again
    right after the line end. A highlight whose own style paints blank cells
    closes there as before and opens again at the next text it matches.
  - For a CR or LF the terminal runs inside an escape or control sequence,
    such as `ESC [ 3 LF 1 m`, it closes before the whole sequence, which is
    kept whole, and opens again right after it.

  Text is drawn in the same colors as before. Piped and file output,
  `profiles test` and the library's `Highlighter::highlight*` are unchanged.

  What this covers, and what it does not:

  - The rule is for CR and LF only. VT and FF keep their bytes, because some
    terminals draw them as characters.
  - In a UTF-8 locale a sequence opened by a raw `9B` is text to the
    terminal, so only the line ends inside it leave the highlight. When the
    highlight already closes before such a sequence, because it saves the
    rendition or fills blank cells, its bytes are kept. A terminal that reads
    8-bit bytes behind a UTF-8 locale, the mismatch `SECURITY.md` describes,
    runs that sequence as a control, and the bytes now written inside it end
    it early, so it is drawn differently than before.
  - Only a whole control sequence that is ASCII after its introducer counts.
    The introducer is `ESC`, or a raw `9B` where the locale is not UTF-8.
    `C2 9B`, a sequence with a byte from 80 to FF after its introducer or cut
    short by one before its final byte, and the payload of an OSC, DCS, SOS,
    PM or APC string keep their bytes, even when a line end sits in the
    sequence.
  - `CSI K` right after a line feed still erases in the color of a
    foreground-only highlight that meets such a program rendition, as before;
    that is a separate issue. A highlight whose style paints blank cells
    already closes before it.
  - A line that wraps at the right margin has no byte to act on and is drawn
    as before.

## 1.4.0 - 2026-11-27

### Changed

- When the wrapped command is killed by SIGINT, PrismTTY now ends by SIGINT
  too instead of exiting with status 130, unless it had already been sent
  another signal or was started with SIGINT ignored or blocked (both below).
  A shell stops a loop typed at its prompt after one Ctrl-C only when the
  command it waited for was killed by SIGINT, so
  `for i in 1 2 3; do prismtty -- sleep 10; done` went on to the next round
  after Ctrl-C; at a zsh or bash prompt, and in a PrismTTY session around an
  interactive shell, it now stops as it does without PrismTTY.

  How PrismTTY ends depends on where the SIGINT went. When it reached the
  command without passing through PrismTTY (Ctrl-C typed while PrismTTY
  passes the keyboard on, or `kill -INT` sent to the command), PrismTTY first
  draws the command's last output, restores the terminal and removes its pid
  file. When PrismTTY itself was sent the SIGINT (Ctrl-C with stdin
  redirected, or `kill -INT` sent to PrismTTY) and the command died from the
  SIGINT that PrismTTY passed on, the end is as abrupt as the exit with 130 it
  replaces: PrismTTY restores the terminal, collects the command and removes
  its pid file, and output not yet drawn can be lost, such as a last line
  without a line end, the reset of an open highlight colour, or its record in
  the trace. When PrismTTY had been sent a different signal, such as SIGTERM,
  and the command then died from SIGINT, PrismTTY still exits with 128 plus
  the signal it was sent.

  What a caller sees. In a shell, `$?` is still 130. Where PrismTTY now ends
  by SIGINT, a program that reads the wait status itself sees a death by
  SIGINT: Python's `subprocess` reports -2 instead of 130, and `make` prints
  `Interrupt` instead of `Error 130`.

  What stays as before. A command killed by SIGQUIT, SIGTERM, SIGHUP or a
  crash is still reported as 128 plus the signal, and any signal other than
  SIGINT that PrismTTY is sent still ends it with 128 plus that signal. A
  command that handles SIGINT and exits keeps its exit code when the SIGINT
  reached it directly; when PrismTTY was sent the SIGINT and passed it on,
  PrismTTY exits with 130, as before. When SIGINT was ignored or blocked as
  PrismTTY started (`cmd &` in a script ignores it), PrismTTY still exits with
  130. A script that runs with the terminal on its stdin (`bash loop.sh`), and
  a zsh or dash pipeline where PrismTTY is not the last command, still go on
  after one Ctrl-C: those shells stop only when they receive the Ctrl-C
  themselves, and while PrismTTY holds the terminal in raw mode it is
  PrismTTY that receives it.

### Fixed

- The generic profile's good operational state rule (the green `up`,
  `connected`, `enabled` and so on) is no longer switched off for the
  session by a line that holds `not` and a long run of blanks. Where PCRE2
  runs without its JIT compiler (a library built without it, or a system
  that refuses it), `not` followed by 7,142 or more spaces, tabs or hyphens
  (measured with PCRE2 10.42 and 10.49) and no good-state word made the rule
  hit its match limit, and PrismTTY printed `rule 'good operational state'
  stopped after a bounded regex match error` and stopped coloring those
  words. With JIT the same took about 50,000 blanks in one buffer, more than
  one read holds. The rule now takes the
  blanks after `not` in one step. It matches exactly what it matched before,
  and its pattern is one byte shorter, so every configuration of your own
  that fit beside the generic profile still fits.

### Documentation

- Both READMEs and SECURITY.md now state limits that are left as they are:
  rules match bytes, and `(*UTF)` at the start of a rule matches characters;
  `--show-profile` reports no selection that stays `generic`; `--sanitize` and
  `--strip-ansi` guard a terminal that reads UTF-8 and do not remove every
  control; PrismTTY owns `pids.d` and `reload` in its runtime directory, and
  in a temporary directory other users can write to, a user who creates
  `prismtty-<uid>` first disables live reload for your sessions; highlighting
  of a file or pipe pauses while the alternate screen is active; with stdin
  redirected and the output on a terminal, a recording of that terminal holds
  CR CR LF, on purpose.

## 1.3.17 - 2026-11-27

### Fixed

- On Linux, a wrapped command killed by a real-time signal (32 to 64) is
  reported with exit status 128 plus the signal number, like any other signal
  death. PrismTTY printed `terminal mode error: EINVAL: Invalid argument` and
  exited 1: on Linux 6.8, `prismtty -- sh -c 'kill -50 $$'` gave 1 with that
  message and now gives 178 with no message. macOS has no real-time signals
  and is not affected.

  A failure to collect the command's exit status is now reported as an I/O
  error instead of a terminal mode error.

## 1.3.16 - 2026-11-26

### Fixed

- On Linux, a wrapped session ends when the wrapped command exits, as it
  already does on macOS. A background job that the command left behind with
  the terminal still open kept the session going: `sleep 600 &`, `disown` or
  `setsid` before `exit` in a wrapped shell, or anything that ignores SIGHUP.
  The terminal stayed in raw mode, Ctrl-C only printed `^C`, and the exit
  status came only when the job let the terminal go. On Linux 6.8,
  `prismtty -- sh -c 'trap "" HUP; sleep 6 & exit 3'` returned after 6 s; it
  now returns at once with status 3, and everything the command itself
  printed is drawn first. PrismTTY sends no signal to what the command leaves
  behind.

  This needs Linux 4.12 or newer. On an older kernel, and on WSL1, nothing
  changes and such a job still keeps the session open, because there the
  command's last output could be missed. The same holds when PrismTTY has no
  file descriptors to spare for it.

  What it costs. Output that a surviving background job writes after the
  command has exited may not be shown, as on macOS. Launchers that start a
  program in the background and exit (`ssh -f host cmd`, `setsid -f`,
  `sudo -b`, a script that ends in `cmd &`) end the session when the launcher
  exits, as they already do on macOS. A command that switches its terminal to
  a line discipline other than the ordinary one (N_TTY) and holds output back
  there can lose bytes it wrote. With file descriptors nearly exhausted, a
  later configuration reload can find none left.

### Documentation

- Both READMEs show how to page a wrapped command's output,
  `ptty cmd </dev/null | less -R`, and say what happens without the redirect:
  PrismTTY and the pager both read the keyboard and set the terminal's modes,
  and quitting the pager while the command still runs can leave the terminal
  in the pager's mode until `stty sane`.
- Both READMEs describe when a Linux session ends.

## 1.3.15 - 2026-11-26

Version 1.3.14 was tagged but never published. One of its new tests compared
two lists of descriptors in the order `ls` printed them, and failed in the
release workflow's macOS jobs, where the runner leaves more descriptors open
and `ls` needs a second row. The test now sorts both lists; nothing else
differs from what 1.3.14 would have been.

### Fixed

- Descriptors above 2 that the caller passes reach the wrapped command, as
  they do without the wrapper: `ptty cmd 3<file`, `--passphrase-fd 3`, and
  process substitution (`ptty diff <(a) <(b)` read "/dev/fd/63: Bad file
  descriptor"). Both ways of starting the command removed them: the PTY
  library closes everything above 2, and since 1.3.0 the start with a
  redirected stdin matched it on purpose. This release reverses that choice.
  A descriptor the caller marked
  close-on-exec still does not reach the command, and nothing PrismTTY opens
  itself does (the PTY, the `--trace-io` file, its signal pipe).
- A command that cannot be executed after it was found no longer aborts when
  started from a terminal. A program built for another CPU type on macOS, or
  a script whose interpreter is itself a script with a missing interpreter on
  Linux, ended with "fatal runtime error", exit 134 and, on macOS, a crash
  report. PrismTTY now prints its usual "Unable to spawn" message and exits
  with the shell's code, 126 or 127. A program for another CPU type gets 126
  with stdin redirected as well, where it got 1.
- When the current directory cannot be entered and `HOME` is not UTF-8, a
  command started with stdin redirected runs in the account's home directory,
  as it already did from a terminal, instead of failing. From a terminal, an
  account whose home directory entry is empty or too long now starts in `/`
  instead of failing.

### Changed

- Spawn failure messages from a terminal use the wording the redirected path
  already had; the exit codes of these failures are unchanged. A missing
  absolute path reads "because it does not exist" instead of "because it
  doesn't exist on the filesystem (ENOENT: No such file or directory)", a
  path that is not executable loses its "(EACCES: Permission denied)", an
  empty command name no longer lists each `PATH` entry as a directory, and a
  failure to enter the directory or to take the terminal names the program.

### Security

- PrismTTY is not a descriptor isolation boundary, and SECURITY.md now says
  so. Because of the first fix above, every descriptor the caller leaves
  inheritable reaches the wrapped command, including ones a terminal, IDE,
  multiplexer or launcher leaked to the shell that starts PrismTTY, exactly
  as they reach a command started without it. Up to 1.3.13 they were closed.
  A launcher that holds sensitive descriptors should close them or mark them
  close-on-exec before it starts PrismTTY.

### Documentation

- Both READMEs say that descriptors the caller passes reach the command.

### Known limits

- On a system where the PTY's name cannot be found (seen with musl in a
  chroot without `/proc`), a command started from a terminal is still
  started the old way: where `/dev/fd` can be listed the caller's
  descriptors are closed and a failed execution still aborts.

## 1.3.13 - 2026-11-25

### Fixed

- Wrapped output saved to a file has the line ends the command wrote.
  `ptty make > build.log` saved CR LF line ends, because the command writes
  to a PTY and a new PTY puts a CR in front of every LF. A CR LF the command
  wrote itself was saved as CR CR LF, on macOS a CR was doubled now and then
  (2 of 20,000 lines in one run), and with the keyboard on a terminal that
  expands tabs the file held spaces in place of tabs. The command's PTY now
  starts with output processing off when PrismTTY's output goes to a regular
  file, or to a pipe while stdin is not a terminal either (a CI job, a
  script, `ptty cmd </dev/null | grep 'x$'`). A command that turns the
  processing back on itself still gets it. Colors are added as before.

  Not changed: output on a terminal, and output in a pipe while the keyboard
  is on a terminal (`ptty ssh host | tee log`, `ptty cmd | grep x` typed at
  a prompt). There the pipe usually leads back to the terminal PrismTTY is
  holding raw, and only the command's PTY knows whether a LF means a new
  line or, for a full-screen program, one row down; the CR LF line ends
  stay. Add `</dev/null` to get the saved form in a pipe typed at a prompt.

  What this costs. A read from the PTY now holds more of the command's text,
  so the place where a read ends moves, and with it what depends on that
  place: a phrase such as `not reachable` can be colored in two parts where
  it was colored whole, and the 64 KiB start sample for profile detection
  reaches further into the output. A program that copies the saved bytes to
  a terminal that is raw at that moment shows lines that do not return to
  the left edge: `tail -f` of the file on the same terminal while PrismTTY
  runs, or a pipe reader on a terminal something else made raw. A terminal
  recorder run under PrismTTY with `> file` (`script -r`) records the same
  LF-only stream; use `| tee file >/dev/null` to keep the terminal stream.

### Documentation

- Both READMEs say when a wrapped command's PTY starts with output
  processing off, and that a pipe typed at a terminal keeps the PTY's usual
  output settings.

### Release and QA

- The publishing job of the release workflow runs in the protected
  environment `release`, which needs an approval and accepts only `v*` tags,
  and the crates.io trusted publisher is tied to that environment. Before,
  a `v*` tag on any commit could publish with that commit's own copy of the
  workflow. Creating a `v*` tag is limited to repository admins.
- The privacy scan reads the text chunks (tEXt, zTXt, iTXt) of PNG files,
  which CONTRIBUTING.md requires generated images to carry, and runs under
  `LC_ALL=C`, so a text line with one byte that is not UTF-8 no longer hides
  a private path or address on that line. The reader follows no symbolic
  link and fails the scan, naming the file, on a malformed or oversized PNG.

## 1.3.12 - 2026-11-25

### Security

- A relative home directory is ignored when PrismTTY looks for its
  configuration and profiles, as a relative `XDG_CONFIG_HOME` already was.
  With `HOME=somedir`, `~/.chromaterm.yml` and `~/.config/prismtty/profiles.d`
  were read from the current directory. Now every home-derived location is
  skipped, silently; an absolute `XDG_CONFIG_HOME`, `/etc/chromaterm` and
  `-c` still apply. This also covers a relative home returned by the account
  database when `HOME` is unset or empty.
- After a single shift (`ESC N`, `ESC O` or their C1 forms), each C0 control
  made the escape scanner walk back over the bytes it had buffered. A crafted
  50 MiB input took 147 s and now takes 0.86 s; the output is the same in
  default mode, with `--sanitize` and with `--strip-ansi`.

### Fixed

- A `not` that is drawn before its next word is drawn in the bad-state
  color. `status not connected` cut between `not ` and `connected`, by the
  end of a read or because `connected` was held back as the last word of a
  read, was drawn as a plain `not` and a green `connected`; the phrase is
  red when it arrives whole. The built-in bad-state rule now also matches a
  `not` followed by blanks or hyphens at the end of the text being matched.
  The green `connected` after it stays, and nothing changes about when text
  is drawn. The rule cannot see why the text ended, so a `not` is red
  wherever it ends a matched piece, whatever follows: a read that ends right
  after it or inside the next word (`file not ` then `found`), a last line
  without a line end, and on a terminal a line that ends in `not ` or
  `not-`. At a shell prompt that includes a pasted multi-line command with a
  line ending there, and a long command that wraps right after the letters
  `not`, also inside a word such as `notable`. On 2.7 MB of manual pages
  with 2,432 such phrases, 2 are red from a file and 14 in a terminal burst.
  The generic profile keeps its nine rules; its pattern text grows by 15
  bytes, which counts toward the 262,144-byte limit shared with your own
  rules.
- One invalid byte in the output no longer switches a `(*UTF)` rule off for
  the rest of the session. PCRE2 refuses a subject that is not UTF-8, and a
  rule that fails is switched off; a stray byte from line noise or a binary
  file did that to every UTF-mode rule. The rule now stays on, and in the
  read that holds the byte the search goes on behind the last invalid byte.
  A rule whose match depends on where the search starts (its text contains
  `\G`, `NOTEMPTY_ATSTART` or `(*COMMIT`) is not searched behind the byte,
  so it cannot color what it does not match. For such input there is no
  "stopped after a match error" warning, `--benchmark` counts no match
  error, and the library's fallible calls return `Ok`. A match-limit error
  still switches a rule off. This does not make UTF-mode rules work on
  output that is mostly not UTF-8; byte-mode rules, the default, are the
  choice there.
- The alternate-screen and save modes (47, 1047, 1048, 1049) are read as
  numbers, so `CSI ? 01049 h` is the switch. The parameter list follows
  xterm's 30 slots: past the 30th, digits run on into it across semicolons.
- A configuration refused for a reason that is not a regex (too many rules
  or pattern bytes; a color naming a capture group its pattern lacks) is
  reported in its own words. It was printed as a rule that failed to
  compile, with the message as the rule's name and an invented PCRE2 error.
  Real compile errors are unchanged. `HighlightError` keeps its one variant
  and two fields.
- `PRISMTTY_RUNTIME_DIR` set to an empty value is named as the reason live
  reload is off for the session, instead of "No such file or directory".

### Documentation

- Both READMEs name the system temporary directory as the default place of
  the runtime directory (`$TMPDIR` when set, `/tmp` on Linux without it),
  and say that only sessions watching the same runtime directory see a
  reload request.

### Release and QA

- `scripts/package-release.sh` refuses a binary that does not load the
  system's PCRE2. The README says the release binaries load PCRE2 at run
  time; when `pkg-config` does not find `libpcre2-8`, `pcre2-sys` builds its
  bundled copy in without a word, and such a build passed every check.
- The release workflow checks draft releases for the tag before it
  publishes. The existing check asks for the published release by tag and
  cannot see a draft; the publish action reuses an existing draft and
  publishes what it holds. A second step in the publish job lists every
  release for the tag and holds each to the same rule: every asset named in
  the manifest, with the manifest's digest.

## 1.3.11 - 2026-11-23

### Security

- In a UTF-8 locale, a control sequence opened by a raw byte `9B` is no
  longer taken for the program's own. A UTF-8 terminal does not run it: the
  byte is an invalid one and the rest is text. PrismTTY learned a rendition
  from it all the same, and the bytes it writes to end a highlight then
  restored that rendition as a live `ESC [ ... m` the program never sent:
  `x <9B>8m link up` got `ESC[8m` (conceal) after each highlight. Such a
  sequence now sets no color, is not treated as a color when a prompt echo
  is neutralized, and switches no screen (an inert raw `9B ?1049h` used to
  turn highlighting off for everything after it). The 7-bit form, the
  two-byte form `C2 9B` and every non-UTF-8 locale are unchanged. Whether
  the terminal reads UTF-8 stays decided once, at startup; `SECURITY.md`
  says what that costs when a program switches the terminal out of UTF-8.
- Output that ends a read in a long run of cut-off escape sequences is no
  longer dropped. The scan that finds where to cut a read gives up after
  eight passes; a read that ended in more than eight cut-off controls in a
  row was carried whole, text included, and thrown away once the carry
  passed 16 KiB. A crafted 24,586-byte input holding 1,063 lines came out as
  one line, in default mode, with `--sanitize` and with `--strip-ansi`. The
  read is now cut in front of the run, so the text before it is written.
- When that carry passes 16 KiB while a string control (OSC, DCS, SOS, PM or
  APC) is still open, the rest of the string is discarded up to its
  terminator. Before, the introducer went with the carry and the payload and
  its BEL were written as text, also with `--sanitize` and `--strip-ansi`.
  Not covered: in default mode a string under its own 16 KiB limit that
  directly follows such a run is dropped in some read splits and passed when
  it arrives in one read.

### Fixed

- A C0 control inside a color sequence that PrismTTY removes from a prompt's
  command echo is kept. A terminal runs such a control where it stands; the
  whole sequence was removed as one unit, so for `host# show ESC[3 LF 1m run`
  the line feed was lost and the terminal showed one line where the source
  drew two.
- The built-in Unix prompt rule (`linux-unix`) and PAN-OS prompt rule
  (`palo-alto`) stop at a carriage return, as they already did at a line
  feed. A `user@host:` followed by a carriage return was colored through to
  a `#`, `$` or `>` later in the same read. For the Unix rule this was also
  slow: 16 MiB of repeated `a@a:` lines ending in a carriage return took
  4.7 s and now take 0.8 s.

## 1.3.10 - 2026-11-21

### Fixed

- The Homebrew formula installs when another package already owns `ct` in
  the prefix (the `chart-testing` and `coreos-ct` formulae install one).
  Homebrew's link step failed on that file and then left the whole formula
  unlinked, so there was no `prismtty` and no `ptty` either. The formula now
  looks at the prefix when it is installed: with another `ct` there, its own
  goes under `libexec`, the completions it installs no longer claim the name,
  and the install ends with a note on how to reach it; with none, everything
  is installed and linked as before. An install that already stopped at the
  link step stays unlinked across the upgrade, as Homebrew keeps that state:
  run `brew link prismtty` once. Not covered: the other package installed
  after PrismTTY (its link step then stops at PrismTTY's `ct`; the README
  gives the two commands that sort it out), and a `ct` that appears or goes
  away later, which takes a `brew reinstall`.

## 1.3.9 - 2026-11-21

### Release and QA

- The Linux release archive is built on Ubuntu 22.04 (image pinned by
  digest, packages from a dated archive snapshot) instead of the release
  runner. Its binaries require glibc 2.34, where the binaries up to 1.3.8
  required 2.39, and any x86-64 CPU. One script builds the archive, measures both
  requirements and runs the three binaries on a stock Ubuntu 22.04; pull
  request CI and the release workflow both call it, so a tag never builds the
  archive in a way a pull request has not already run.

## 1.3.8 - 2026-11-21

### Fixed

- `prismtty profiles test` with both `--sanitize` and `--strip-ansi` drops a
  GNU screen or tmux title string whole, as the stream does. It printed the
  title's text; no escape survived, so no title could be set.
- A C0 control or DEL inside a CSI no longer hides the sequence. The switch
  to and from the alternate screen is read with such a byte inside it:
  before, a full-screen program's frame was colored, or highlighting stayed
  off after the program left. `CSI s` with one no longer saves the highlight
  color. A private-mode sequence that holds a second `?` or an intermediate
  byte, which a terminal ignores, is no longer taken as a screen switch or
  as a rendition save.
- A highlight ends before xterm's XTPUSHSGR (`CSI # {`, `CSI # p`) saves the
  rendition and starts again after it, so the pop no longer brings the
  highlight color back onto text that did not match. The xterm stack itself
  is not tracked.
- With `--trace-io` in several sessions on one file, the first session that
  cannot fit a line fills the file to its limit, with a line of dashes and
  then the truncation marker (dashes only when the marker does not fit), and
  a session that finds the file at its limit adds nothing. Each session
  checks the size and writes under a lock on the file; while the sessions
  get that lock, nothing follows the marker and the limit is not passed. A
  session that cannot take the lock within 50 ms writes without it from then
  on, and one of its lines can still land below another session's marker.
- Shell completion stops at the wrapped command. Bash no longer offers
  PrismTTY's options or `profiles` after `--` or the first command word, and
  zsh offers the `profiles` subcommands right after `profiles` and hands a
  wrapped command's words to that command's own completion. At the first
  word zsh offers commands found on PATH next to files and `profiles`. Known
  limits: completion offers nothing of PrismTTY, and the command still runs
  as typed, after a quoted option such as `'-b'` in both shells, and in Bash
  after a redirection between an option and its value, after an empty
  attached value such as `-p=`, and after a config file named `=` or, on
  Bash 4 and later, one whose name ends in `:`.
- `sftp` and `mysql` started by name from the local shell no longer switch
  the session to the Cisco profile: while such a client runs, its own prompt
  (`sftp> `, `mysql> `) is not read as a device prompt. Limits: a device
  whose own prompt is `sftp> ` or `mysql> ` is not detected by its prompt
  after such a command, and the memory of the client is not cleared at a
  local prompt PrismTTY does not recognize.
- `connect` is no longer green in `connect to host`, the wording of a failed
  ssh or telnet connection. The verb elsewhere and `connected` keep their
  color.

### Documentation

- `SECURITY.md` states the 16 KiB limit on string controls in the default
  mode, where it said escapes pass through unchanged, and what the filtered
  modes show after an ESC followed by 1 KiB of C0 controls and a read
  boundary. The README notes that a trailing byte that could begin a UTF-8
  character is shown with the next output or at the end of output.
- The README says that after a connection failure PrismTTY does not
  recognize, a later close line naming an outer host can leave the remote
  profile active.

### Release and QA

- Release packaging refuses a binary that is not an executable for the
  target it is asked to package. The object format and the CPU must match,
  and a Linux binary must be 64-bit, little-endian and marked for the System
  V or GNU/Linux ABI, so a macOS, FreeBSD or x32 binary cannot ship in the
  Linux archive and one Mac CPU's binary cannot ship under the other's name.
  A universal Mach-O is refused under both macOS names.

## 1.3.7 - 2026-11-19

### Security

- Remove the window title string of GNU screen and tmux,
  `ESC k title ESC \`, under `--sanitize`. Only those two read it as a
  string, so it passed as a two-byte escape followed by text, and a remote
  device could set the title of the pane PrismTTY ran in. It is now found,
  carried across reads and dropped like an APC string, and
  `strip_string_escapes` in the library removes it too. Without `--sanitize`
  nothing changes.

## 1.3.6 - 2026-11-19

### Fixed

- Color a phrase that spans the 64 KiB boundary in a `profiles test` fixture.
  The fixture is highlighted 64 KiB at a time, and a rule that matches across
  words, such as `not connected`, saw the two halves apart. Each piece now
  ends at a line end. A rule that matches across a line end can still lose a
  match where one piece ends and the next begins.
- Install the bash completions for `ptty` and `ct` from the Homebrew
  formula. bash-completion loads a file on demand by the name of the command
  being completed, and only one named `prismtty` was installed.
- Package release archives from the cargo target directory in use.
  `scripts/package-release.sh` copied from `./target/release` even when
  `CARGO_TARGET_DIR` sent the build elsewhere.

## 1.3.5 - 2026-11-19

### Fixed

- Unwind both connections when two nested ones close in one read. The first
  teardown line ended its connection and the second was dropped, so local
  output kept the outer device's profile. Only a line that names its
  connection goes on to the next one, and the close line that follows a
  `logout` counts with it as one exit.
- Keep a vendor's detection in live sessions when a `profiles.d` profile
  takes a built-in profile's name. A profile file cannot set the runtime
  metadata that detection uses, so such a profile, written to change a
  built-in's colors, was never selected inside a wrapped shell again.

## 1.3.4 - 2026-11-19

### Fixed

- Keep a highlight's color off the spacing between two highlighted columns
  where the program draws blank cells in the foreground color. In an
  interactive stream a foreground-only highlight stays open across spaces and
  tabs; under the program's own inverse, underline, strike-through or
  overline that showed as colored blocks or lines between the columns.
- Color an IPv6 address that ends in a dotted IPv4 address as one address:
  IPv4-mapped (`::ffff:` and an IPv4 address), NAT64, and the form written
  out in full. Only the IPv4 tail was colored.
- Hold two sessions that trace to the same `--trace-io` file to one 64 MiB
  limit. Each counted only its own lines, so together they could write twice
  the limit.

## 1.3.3 - 2026-11-19

### Fixed

- Highlight again after a full reset (`ESC c`) inside the alternate screen.
  The reset puts the terminal back on the normal screen, but PrismTTY kept
  reading later output as alternate-screen drawing and left it unhighlighted
  for the rest of the session.
- End diagnostics in CR LF on a terminal in raw mode. A wrapped session puts
  the terminal in raw mode, where a bare LF does not return the cursor to the
  left edge, so the `--show-profile` report, a rule warning or the
  `--benchmark` report left the lines after it starting mid-screen. No CR
  is added where the terminal adds it itself or would turn it into a
  second LF.
- Keep a rule in UTF mode (`(*UTF)`) that can match nothing working on
  multi-byte text. The search resumed one byte after an empty match, inside
  a character, which PCRE2 refuses: the rule was reported as stopped and the
  rest of the word went unmatched. The search now moves on to the next byte.
- Report where a rule fails to compile as an offset in the rule as written.
  The offset counted the 40 characters of match limits PrismTTY compiles in
  front of each rule.
- Name the fixture when `profiles test` cannot read it.
- Remove a pid file named above any process id. `4294967295` was read as -1,
  which stands for every process, so it counted as a live session for
  `--reload` and was never pruned.
- Say in the `inherits` field docs that a profile's own rules come first and
  its parents' rules after.
- Correct the 1.3.2 entry below: past 16 KiB after a stray byte, the rest of
  that line is dropped in default mode too.

## 1.3.2 - 2026-11-18

### Fixed

- Stop one stray byte from hiding the rest of a session on a UTF-8 terminal.
  A raw byte `90`, `98`, `9D`, `9E` or `9F` that is not part of a UTF-8
  character opens a string control on a terminal that is not in UTF-8 mode,
  so PrismTTY treated it as one in every mode: with no terminator after it,
  all later output was withheld, discarded past 16 KiB and dropped at the end
  of input. A legacy code page (`9E` is a letter in CP1250), a UTF-8 character cut
  in half and serial line noise all produce such a byte, and a plain-text
  device never sends the terminator. When the locale says UTF-8 (the first of
  `LC_ALL`, `LC_CTYPE` and `LANG` that is set names a UTF-8 codeset), such a
  string now ends at the end of its line. One stray byte then costs at most
  the rest of that line: in default mode the line passes through unchanged
  and unhighlighted when no more than 16 KiB follow the byte on it, and what
  follows the byte is dropped past that; `--sanitize` removes the byte and
  the rest of the line. The lines after it are shown and highlighted as
  usual.
  - Without a UTF-8 locale, an unset one included, nothing changes: the byte
    is the control an 8-bit terminal reads and the string runs to its
    terminator. The library also behaves as before.
  - What is left on that one line: its text after the byte is held until the
    line ends, so a prompt with a stray byte in it shows its tail when the
    next line break arrives, and a last line with no line break loses the
    text after the byte.
  - A control written as the UTF-8 character (`C2 9D`) is a control on a
    UTF-8 terminal too, and is handled as before.

## 1.3.1 - 2026-11-17

### Security

- Apply `--sanitize` and `--strip-ansi` to `profiles test`. It printed its
  fixture without the filtering the two flags ask for, so an OSC 52 clipboard
  write in a captured fixture still reached the terminal under `--sanitize`.
- Refuse a `--trace-io` file that has a second hard link. The trace was
  appended to whatever else the path was linked to.
- Open the reload marker and pid files without blocking. A FIFO in place of
  the marker made `prismtty --reload` hang forever.

### Fixed

- Treat only real SGR controls as the program's colors. Any CSI ending in `m`
  counted, so xterm's key modifier option (`CSI > 4 ; 1 m`, XTMODKEYS) was
  tracked as bold and the text after each highlight was redrawn bold or
  faint. The same control was deleted from the stream when a prompt's colors
  were neutralized, so the terminal never changed how it reports keys. A
  control with an intermediate byte (`CSI + 31 m`) is no longer read as a
  color, and a C0 control inside an SGR no longer loses the color.
- Reject surplus arguments to `profiles list`, `show`, `validate` and `test`.
  `profiles validate good.yml broken.yml` reported success having read only
  the first file.
- Write the `--benchmark` report, and the `--show-profile` report for a
  forced profile, when the input is empty.
- Wait for input when the filter inherits a non-blocking stdin, instead of
  exiting with "Resource temporarily unavailable".
- Let an exclusive rule style all of its own capture groups. With colors on
  groups 0 and 1, group 1 was skipped because group 0 had already protected
  the span.
- Match a profile's strong signals only where they begin a word. `rhel`
  inside `ServerHello` selected linux-unix, `ios xe` inside `BIOS XE` and
  `asa version` inside `NASA version` selected cisco, and `pan-os` inside
  `japan-osaka` selected palo-alto. A model or version glued on after the
  signal (`Nexus9000`, `rhel9`) still matches.
- Skip SS3 keys whole in `--local-echo`. An arrow or function key sent in
  application mode (`ESC O A`) was echoed as `OA`.
- Reject a numeric capture key that does not fit the platform's `usize`. On
  a 32-bit build 4294967296 wrapped to group 0.

### Profiles

- generic: a good-state word after `not` is no longer green (`not reachable`,
  `not  connected` in aligned columns, `not valid`), and `not reachable` is a
  bad state like `not connected`.
- generic: bare `den` (a site code, a word) is no longer painted as a denial;
  `deny`, `denies` and `denied` still are.
- generic: an IPv6 zone that holds `-`, `.` or `_` (`%eth-0`, `%bond0.100`)
  is colored with its address instead of splitting it.
- arubacx and fortinet: configuration-context prompts (`switch(config)#`,
  `FGT (global) #`) are colored like the plain prompt.
- palo-alto: aggregate interfaces (`ae1`, `ae1.107`) are colored.

### Library API

- `strip_ansi` keeps the format effectors (backspace through carriage return)
  found inside a CSI or a plain escape sequence, as a terminal runs them.
  It used to drop them with the control, which joined the lines around an
  escape cut short at a line end. Controls inside a string control are still
  removed with it.

### Release and QA

- CI builds the crate package (`cargo package --locked`), so a file missing
  from the `include` list fails a pull request instead of a release tag.
- The privacy scan checks file content and paths separately for IPv6
  addresses. It scanned `git grep` records whole, so a line number joined an
  address that began the line. It also checks zero-padded IPv4 octets.
- A prompt matcher test used a real local account name; it is synthetic now.
- The READMEs no longer say a release archive contains its checksum, and
  name the PCRE2 runtime requirement.

## 1.3.0 - 2026-10-22

### Security

- Strip an OSC, DCS, SOS, PM or APC string under `--sanitize` when its
  introducer follows ESC plus C0 controls or DEL (for example
  `ESC NUL ] 2;title BEL`). A terminal executes those controls and still opens
  the string, so the sequence used to pass through.
- End an OSC, DCS, SOS, PM or APC string at any ESC that does not begin ST, as a
  terminal's parser does. A stray `ESC ]` no longer holds back and then
  discards all later output, an OSC ended by a CSI is no longer dropped at end
  of input, and an oversized discarded string ends where the terminal ends it.
- Stop holding bytes that can never form a character (such as `F0 8E` or
  `E0 9D`) as a partial character at the end of a read. Their C1-valued bytes
  are treated as the controls a terminal sees, so `--sanitize` and the end of
  input neutralize the CSI, OSC or DCS they start.
- Drop an ESC under `--sanitize` when a non-ASCII byte follows it, looking
  past the C0 controls and DEL that a terminal runs without leaving the
  escape. xterm reads U+00A0 to U+00FF after ESC as the ASCII byte each one
  mirrors, so `ESC U+00DD 0;title BEL` set the window title, and in UTF-8
  mode xterm and Alacritty skip a C1 control there, so
  `ESC C2 80 ] 0;title BEL` did too. The character or C1 control after the
  ESC is kept. A control that the ESC cancelled, such as `ESC [ ? 4`, is
  dropped with it, so the bytes after it cannot complete it.
- Drop every DOCS sequence (`ESC %`) under `--sanitize`, with the controls
  inside it. `ESC % @` can switch xterm out of UTF-8, after which the 9D byte
  in ordinary text such as U+201D opens an OSC.
- Drop a CSI under `--sanitize` when it holds a non-ASCII byte (80 to FF) or
  a C1 control cancels it, and drop any CSI that sets, resets or restores DEC
  private mode 42 (DECNRCM). In UTF-8 mode xterm skips a C1 control inside a
  CSI and reads U+00A0 to U+00FF there as ASCII, so `ESC [ ? 4 U+00B2 h` was
  `CSI ? 42 h`, which switches xterm out of UTF-8 even when its utf8 resource
  is "always". The CSIs programs normally send, such as colors, cursor
  movement, `?25h`, `?1049h` and `?2004h`, pass as before.
- Cap an incomplete run of cancelled controls (repeated `ESC` or `ESC [`) in the
  library's `StreamingHighlighter` from the run's first control, instead of
  keeping it pending without bound and rescanning it on every push.
- Reject config and profile files whose YAML collections nest more than 128
  levels deep before libyaml scans them. A file of nested `[` or `{` within the
  1 MiB read limit could keep startup, `--reload` or `profiles validate` busy
  for minutes, with a wrapped session's terminal in raw mode the whole time. It
  now fails at once with the same `recursion limit exceeded` error as before.
- Bound how far YAML aliases and `%TAG` directives can expand a config or
  profile file. A file under the 1 MiB read limit could otherwise ask for
  gigabytes and abort, leaving the terminal in raw mode. A document is rejected
  once its aliases would add more than 4 MiB, when an alias sits inside the
  collection it names, or when its resolved tags pass 4 MiB. Ordinary anchors,
  such as a shared color, still work.
- Leave the mode of an existing `PRISMTTY_RUNTIME_DIR` alone instead of
  chmodding it to 0700. A runtime directory that other users can write to is
  refused untouched, which only disables live reload.
- Ignore a relative `XDG_CONFIG_HOME`, as the XDG spec requires, so
  `config.yml` and `profiles.d` are never loaded from the current directory.
- Reject a `--trace-io` path that is a FIFO instead of blocking on it with the
  terminal in raw mode.
- Keep prismtty's signal pipe out of the wrapped command and its descendants,
  even where `/proc` and `/dev/fd` are unavailable.
- Document `cargo install --locked prismtty`, so crates.io installs build with
  the dependency versions in the audited `Cargo.lock`.

### Changed

- When stdin is not a terminal, the wrapped command now reads prismtty's stdin
  directly, as with `cmd < file`, while its output still goes through the PTY.
  Piped data reaches the command, every read after the data ends sees end of
  input, and a command that switches its input to raw mode no longer reads a
  stray NUL. Before, piped data was dropped and a command that read stdin
  twice, such as a script with two `read`s, hung.

### Fixed

- Keep the output of a wrapped command that exits right away on macOS, such
  as `ptty printf ok`. When the command finished before prismtty closed its
  side of the terminal, that close waited 0.6 s and discarded everything the
  command had printed, so the session showed nothing and exited 0.
- Run the wrapped command in the caller's working directory instead of
  `$HOME`, and resolve a relative program path such as `bin/tool` against it,
  so `prismtty make` or `prismtty rm -rf build` no longer act on the home
  directory. A deleted working directory is kept as well. Only when the
  directory cannot be entered at all does the command start in `$HOME`, after
  a warning, and when `$HOME` cannot be entered either, prismtty names it and
  exits 1. A relative program such as `bin/tool` now sees `./bin/tool` as
  `argv[0]`.
- Exit with 127 when the wrapped command is not found and 126 when it is a
  directory or not executable, instead of 1. A script whose `#!` interpreter
  is missing or not executable, including a `/bin/sh\r` line from a CRLF file,
  exits 127 or 126 instead of aborting with exit 134. The `#!` line is read
  the way the kernel reads it (on macOS `#` also ends it, and on Linux a name
  that ends past the first 128 bytes is left to the kernel), so a script the
  system would run is never refused.
- Report bad `-p`, `-c`, `profiles.d` and `--trace-io` values before the
  wrapped command starts, so a typo or a broken config never runs part of the
  command.
- Exit quietly with status 141 when the reader of the output goes away
  (`| head`, quitting `less`) instead of printing `Broken pipe`, and stop
  `--help`, `--version`, `profiles` and `--reload` from panicking in that case.
  A closed or hung-up stderr no longer panics session cleanup or stops
  highlighting.
- Wait for the terminal instead of ending the session with "Resource
  temporarily unavailable" when another program left the terminal in
  non-blocking mode, both for typed input and for a burst of output, and in
  stdin mode (`cmd | prismtty`) when the output is that terminal.
- Keep highlighting when the runtime directory cannot be created or used; live
  reload is disabled for that session with a warning.
- Remove a wrapped session's pid file when the session ends on a signal
  (closing the terminal window, `SIGTERM`, `SIGHUP`), and prune pid files left
  by sessions killed outright when the next session registers, so `--reload`
  stops counting them.
- Stop injecting a newline and end-of-file into the wrapped session when stdin
  forwarding ends, which could submit a half-typed remote command.
- Leave signals the caller ignored ignored, so `nohup prismtty -- job` survives
  logout and Ctrl-C no longer ends a `prismtty -- cmd &` started from a script.
- Report the wrapped command's exit status when prismtty inherits an ignored
  `SIGCHLD`, instead of exiting 1 with "No child processes".
- Start the wrapped command when prismtty runs as a session leader without a
  controlling terminal (for example under `setsid`).
- Let a backgrounded prismtty resume with `fg`, leave the shell's terminal
  settings alone while prismtty is a background job (also when it is
  terminated there), re-enter raw mode after an uncatchable `SIGSTOP`, and
  pass resizes made while stopped or starting to the wrapped PTY.
- Draw program output that ends mid-word, such as Cisco `ping` dots or
  `Building configuration...`, after 250 ms of quiet instead of holding it
  until the program writes again. This also applies to `tail -f log | ct` and
  to a wrapped command whose stdin is redirected, when the output is a
  terminal.
- Close the highlight color before waiting for input in pipe mode with a
  terminal as output, so Ctrl-C, Ctrl-Z or typing no longer leave the terminal
  colored.
- Restore the program's own color after a highlight in pipe mode when that
  color was set in an earlier read, instead of resetting to the default for
  the rest of the line.
- Track how the terminal resets, saves and restores the program's color (RIS,
  DECSTR, DECSC and DECRC, parameterless `CSI s` and `CSI u`, and modes 1048
  and 1049, with separate copies for the normal and alternate screens, also
  when C0 controls follow the ESC), so a color the terminal already dropped no
  longer comes back after a highlight. A highlight is ended before such a
  save, so the matching restore cannot paint it onto unrelated text.
- End what one highlight style set before switching straight to another on a
  terminal, so a background, underline or bold (under `--no-minimal-reset`)
  no longer carries onto the next token, the following lines or the terminal
  after exit. Switches between foreground-only styles, which the built-in
  profiles use, are unchanged.
- End background, underline, inverse and strike-through highlights before the
  line ending, before erase or scroll sequences, and before index, next line
  and reverse index, so they no longer bleed across trailing spaces, the next
  line's indentation, erased cells or scrolled-in lines.
- Highlight finished LF-ended lines that contain `%`, `#`, `>` or `$` after
  other text (syslog lines, `df` output, route tables) when reading stdin with
  a terminal as output; they were held back as if they were a typed command.
- Keep a carriage return inside an OSC or other string control from splitting
  a prompt echo, so highlight colors are no longer inserted into a window
  title or hyperlink.
- Keep a device's color code on an echoed prompt line when its escape is what
  closes an unterminated string control, instead of removing it and hiding the
  output that followed.
- Step past a rule match that PCRE2 reports with its start after its end
  (`\K` inside a lookaround on PCRE2 older than 10.38) instead of looping until
  memory runs out.
- Keep an ESC followed by a multibyte UTF-8 character from splitting that
  character; the library calls `Highlighter::highlight_str` and
  `StreamingHighlighter::push_str` no longer panic on it.
- Discarding an oversized string control after a cancelled control no longer
  panics when another oversized string follows its terminator.
- Keep the line ending after a single shift (`ESC N`, `ESC O`, SS2 or SS3) in
  interactive sessions, and stop turning it into `^N` at end of input.
- Make output independent of where a read boundary falls inside a UTF-8
  character after a control, or after a C1 byte inside a terminated OSC
  payload; under `--sanitize` the latter could hide the text after the OSC.
- Return to the local profile when an `ssh` session closes whose host was
  completed with TAB, or whose ssh_config alias names a different `HostName`.
- Keep a typed `ssh` or `telnet` hint through a password, a user name or the
  Enter that redraws a prompt, so nested prompt-only devices are detected and
  closing a nested session no longer unwinds its parent. Nested `ssh` typed on
  a device CLI with trailing keywords (`vrf management`,
  `routing-instance ...`) is still tracked.
- Stop `ssh host command`, `ssh -T`, `ssh -V`, `ssh -O ...`, `ssh -fN ...` and
  `screen -ls` typed at the local shell from leaving a profile frame behind
  that kept later sessions from returning to the local profile.
- Stop local output such as rustc `-->` arrows, a `C# ...` line or a printed
  `done # comment` from switching the local shell to the Cisco or Fortinet
  profile.
- Keep a device identified by its login banner on its profile when another
  vendor's name appears in LLDP or CDP neighbor output or an interface
  description.
- Keep the previous rules, with a warning, when a `profiles.d` profile that
  startup detection picks from the first output fails to compile, instead of
  ending the session.
- Highlight NX-OS `Ethernet1/1`, the IOS XE and IOS XR names `HundredGigE`,
  `FortyGigE`, `TenGigE`, `TwentyFiveGigE`, mGig ports, `Bundle-Ether` and
  four-level IOS XR names such as `GigabitEthernet0/0/0/0` in the Cisco
  profile, and three-level Arista names such as `Ethernet3/1/1` in full.
- Color IOS XR four-part syslog tags such as `%PKT_INFRA-LINK-3-UPDOWN` by
  severity, `permitted` as a good state and `failures` as a bad state.
- Highlight IPv4 and IPv6 addresses followed by a sentence period or ellipsis
  (`Trying 192.0.2.1...`), and the Junos configuration-mode prompt
  `user@host#`.
- Stop coloring part of container image tags and package versions as a port in
  the linux-unix profile (`ubuntu:22.04`, `python:3.12-slim`, `1:2.35`).
- Install the fish completions for `ptty` and `ct` from the Homebrew formula,
  and document how to name completion files for manual installs.

### Performance

- Stop rescanning up to 64 KiB of already-examined output on every read during
  dynamic profile detection, and match banner signals against the lowercased
  window with a substring search. `cat` of a 4 MiB router log in a wrapped
  shell drops from 7-8 s to about 1 s, and a slow trickle of lines no longer
  keeps a core busy.
- Rerun startup profile auto-detection only at a line end, a prompt or every
  4 KiB instead of on every read, which removes per-keystroke work under
  `--no-dynamic-profile` and stops byte-paced pipes from saturating a core.
- Make prompt-echo detection linear on lines packed with Unicode prompt
  markers and on output with many carriage returns: a 512 KB marker flood
  drops from 1.7 s to 0.05 s, and CR-heavy PTY output uses about 35x less CPU.
- Reuse each read's tokenized form, render each style's escape sequence once,
  and skip Unicode and escape-map work for plain text, cutting instruction
  counts by 11-17% on large router dumps.
- Keep splitting a read that ends in a word linear after many string controls
  that share one terminator: 1 MiB of such output took about 5 s and now takes
  0.06 s.
- Split a read that ends in a run of controls, each ending in a character the
  next one cuts off (such as repeated `ESC F0 9F 9C` or `ESC C2`), in about one
  scan instead of up to eight. Such output arriving in small reads is carried
  whole, so every read rescanned the growing carry up to eight times. In a
  release build, 64 KiB of it in 4-byte reads took about 20 s and now takes
  about 4 s. What is emitted and what is carried is unchanged.

### Release and QA

- Check in the interactive echo end-to-end tests that the echo trigger, not
  the 250 ms program-output holdback, drew the tail, so an echo-latency
  regression no longer passes unnoticed.
- Gate the cross-read highlight tests on a FIFO instead of sleeps, and count a
  shell held in `vfork` as stopped in the job-control test, so slow or loaded
  CI runners no longer fail them.
- Add chunking differential tests that feed the same bytes in one read, in
  random reads and a byte at a time, and check that the output matches and
  that `--sanitize` never leaves a string control open.

## 1.2.5 - 2026-08-20

### Fixed

- Recognise a command line rewritten with cursor-movement controls, not just
  with backspaces, so a history recall still draws its whole line under a line
  editor that redraws that way (zsh's `zle` once the edit is more than a few
  columns back). Column-one repaints stay excluded: a bare carriage return,
  `ESC [ G` or a lone `ESC [ K` is what a progress bar emits on every update.
- Track the rewritten line as a flag rather than by searching the retained line
  tail. A rewrite is no longer lost when its controls arrive inside a prompt
  repaint the line tracker skips, or when the line runs past the 512-byte tail
  the highlighter retains; and a line ending inside such a repaint now clears
  the signal instead of letting it arm one spurious early flush on the next
  line.

## 1.2.4 - 2026-08-20

### Fixed

- Draw the whole line when a shell recalls a command from history. Pressing the
  up or down arrow makes the remote line editor backspace over the input line
  and rewrite it; when the recalled command ended mid-token (no delimiter after
  a trailing `=`, for example) everything after that point stayed invisible
  until the next keypress. A tail stranded on a line whose accumulated output
  carries a backspace is now surfaced once the session goes idle, like a prompt
  redraw already was, including when the redraw arrives across several reads.

## 1.2.3 - 2026-08-19

### Security and Reliability

- Keep highlighting the rest of a chunk when one rule exhausts its PCRE2 match
  budget on a pathological span. The scan now resumes after the token that
  failed, for at most four failures per rule per chunk, and the matches found
  around it are applied before the rule is set aside for the rest of the
  session. Previously a single such failure dropped every match that rule had
  found on that chunk.
- Accept a bare carriage return as a line terminator when inspecting remote
  close markers, so devices that end lines with `\r` only still return to
  the local profile when their session closes.
- Resolve styled capture references to group indices when a rule compiles,
  and resolve a duplicate `(?J)` group name to the last group carrying it, as
  PCRE2 does (1.2.2 picked the first).

### Release and QA

- Add a policy test requiring every GitHub Actions `uses:` in the workflows to
  be pinned to a full commit SHA, so a future mutable-tag reference cannot
  slip in unnoticed.
- Write the release version literally into the generated Homebrew formula's
  URLs and drop the explicit `version` line, so Homebrew derives the version
  from the URL and `brew audit --strict` passes without the redundancy note;
  CI and release validation check the URL form instead. Formula generation
  now refuses a version Homebrew cannot derive from the URL: for a hyphenated
  pre-release such as `2.0.0-rc1`, Homebrew reads the `x86_64` in the file
  name and reports version `86.64`, which would outrank every later release.
- Make the Ubuntu build dependency install survive a broken package mirror:
  force IPv4, bound each fetch, retry, and fall back to the canonical Ubuntu
  archive when the runner's regional mirror stops answering.

## 1.2.2 - 2026-08-18

### Library API

- Mark the `cli` module `#[doc(hidden)]`. It is public only so the crate's own
  binaries can call it; it is not part of the supported library API or its
  semver contract.

### Performance

- Reuse PCRE2 match data while applying rules. Each rule ran every line
  through `captures_iter`, which allocates a match-data block and a JIT stack
  per match attempt; whole-match rules now use the regex's pooled match data
  and capture-styled rules reuse one capture block per line. Piped highlighting
  of a synthetic 20k-line router dump with four profiles drops from 0.57s to
  0.14s, and a capture-styled rule that can match empty drops from 4.3s to
  0.16s. Output is unchanged.
- Check for a Unicode prompt marker before decoding when scanning a visible
  line for prompt echo. The scan runs once per candidate prompt end, so long
  lines without a prompt paid a full UTF-8 decode per byte; interactive
  throughput on 8 KiB lines without newlines improves from 1.1 MiB/s to
  4.4 MiB/s.

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
- Close the signal self-pipes only after any in-flight signal handler has
  finished writing to them. Previously a handler that had loaded the pipe
  descriptor just as the session was tearing down could write its byte into a
  descriptor number that had already been reused.

### Profiles

- The generic IPv6 rule no longer paints `::` scope-resolution operators in
  compiler and stack-trace output (`MyClass::Add`, `Foo::dead`, `Ok::<u8, _>`);
  a `::`-led address must start its token, so `::1`, `[2001:db8::1]:443`, and
  `fe80::1` still highlight.

### Packaging

- Build release binaries with fat LTO and a single codegen unit: `prismtty`
  shrinks from 2.28 MB to 1.76 MB with no change in highlighting speed.

### Release and QA

- Install the pinned `cargo-audit` 0.22.1 as a prebuilt binary through a
  SHA-pinned `taiki-e/install-action` in CI and release validation instead of
  compiling it on every run (the supply-chain job was the longest CI job),
  cache Cargo dependencies in the test jobs, and run push CI only on `main`
  so a pull request from a branch in this repository is checked once, not
  twice.
- Require a `SAFETY:` comment on every `unsafe` block
  (`clippy::undocumented_unsafe_blocks`, enforced by the CI clippy gate) and
  document the existing libc PTY, termios, signal, and wait calls.
- Bound the PCRE2 install step in CI and release packaging to 10 minutes and
  give apt fetch timeouts and retries, so a stalled Ubuntu mirror fails the
  job in minutes instead of holding it for hours.

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
