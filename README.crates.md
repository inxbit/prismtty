# PrismTTY

<p align="center">
  <img src="https://prismtty.com/assets/prismtty-logo.svg" alt="PrismTTY" width="760">
</p>

<p align="center">
  <a href="https://prismtty.com/"><img alt="website" src="https://img.shields.io/badge/website-prismtty-0a0b0d?style=flat-square"></a>
  <a href="https://crates.io/crates/prismtty"><img alt="crates.io" src="https://img.shields.io/crates/v/prismtty?style=flat-square"></a>
  <a href="https://docs.rs/prismtty"><img alt="docs.rs" src="https://img.shields.io/docsrs/prismtty?style=flat-square"></a>
  <a href="https://github.com/inxbit/prismtty/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/inxbit/prismtty/ci.yml?branch=main&style=flat-square"></a>
  <a href="https://github.com/inxbit/prismtty/releases"><img alt="release" src="https://img.shields.io/github/v/release/inxbit/prismtty?style=flat-square"></a>
  <a href="https://github.com/inxbit/prismtty/blob/main/LICENSE"><img alt="license" src="https://img.shields.io/github/license/inxbit/prismtty?style=flat-square"></a>
</p>

PrismTTY makes dense SSH, network-device, and Unix/Linux terminal output easier
to scan in real time. It is a fast ChromaTerm-style CLI wrapper with
network-focused built-in profiles.

Website: [prismtty.com](https://prismtty.com/).

## Quick Demo

<p align="center">
  <img src="https://prismtty.com/assets/prismtty-terminal-demo.svg" alt="Animated PrismTTY terminal demo">
</p>

## Preview

Real PrismTTY output over synthetic fixtures (invented hosts, documentation-range addresses):

<p align="center">
  <img src="https://prismtty.com/assets/prismtty-terminal-preview.svg" alt="Highlighted PrismTTY terminal output">
</p>

<p align="center">
  <img src="https://prismtty.com/assets/prismtty-profile-switching.svg" alt="Dynamic PrismTTY profile switching">
</p>

## What This Is / What This Is Not

PrismTTY is a live terminal-output highlighter for shells, SSH sessions, pipes,
and logs.

It highlights output that is already flowing through your terminal, including:

- prompts
- interfaces
- IP addresses
- protocol state
- counters
- severity
- common vendor terms

PrismTTY is not:

- an NMS
- a configuration-management tool
- a source of truth
- a SIEM
- a log platform
- a device inventory system

It does not log in to devices by itself, collect inventory, push configuration,
or store operational data.

## Install

### Homebrew

Install the latest release from the PrismTTY Homebrew tap:

```sh
brew install inxbit/tap/prismtty
```

The Homebrew formula installs `prismtty`, `ptty`, `ct`, bundled profiles, and
shell completions. It also installs the required PCRE2 runtime dependency.

If another package already owns `ct` in the Homebrew prefix, the formula leaves
its own `ct` unlinked under `libexec` and says so at the end of the install;
`prismtty` and `ptty` are linked as usual.

### Cargo

Install from crates.io:

```sh
cargo install --locked prismtty
```

Cargo builds PrismTTY from source and installs the command binaries. You need
Rust 1.85 or newer plus PCRE2 and `pkg-config` available on your system.
`--locked` builds with the dependency versions audited for the release.

For `cargo install` on macOS, install the native build dependencies first:

```sh
brew install pcre2 pkg-config
```

For `cargo install` on Debian/Ubuntu, install:

```sh
sudo apt-get install libpcre2-dev pkg-config
```

Cargo installs the binaries only. Shell completions for zsh, bash, and fish
live in the repository's
[`completions/`](https://github.com/inxbit/prismtty/tree/main/completions)
directory; copy the file for your shell into its completions directory. Fish
loads a completion file only by the command's name, so also link
`prismtty.fish` as `ptty.fish` and `ct.fish`. For the bash-completion on-demand
directory, install `prismtty.bash` there as `prismtty`, `ptty`, and `ct`, or
source it from `~/.bashrc`.

### GitHub Release

Prebuilt release archives and checksums are available on the
[GitHub releases page](https://github.com/inxbit/prismtty/releases).

Each release archive contains the binaries, license/readme files, example
profiles, and shell completions. Its `.tar.gz.sha256` checksum is a separate
file next to it on the release page. The binaries load PCRE2 at run time: from
Homebrew (`brew install pcre2`) on macOS, and from the system `libpcre2-8` on
Linux.

Installed commands:

- `prismtty`: highlighter CLI, profile tools, config reload, and benchmarks.
- `ptty`: launch an interactive command through PrismTTY.
- `ct`: short compatibility alias for `prismtty`.

## Usage

```sh
ptty /bin/zsh
ptty ssh router.example.net
prismtty --profile cisco < show-tech.txt
prismtty profiles test cisco fixtures/cisco.txt
prismtty --reload
```

The recommended interactive workflow is to start one wrapped shell from your
terminal profile:

```sh
ptty /bin/zsh
```

From inside that shell, run normal `ssh`, `telnet`, or console-wrapper commands.
PrismTTY dynamically switches profiles from observed login banners and prompts,
then keeps the selected remote profile locked for the session.
If a connection attempt fails with a message PrismTTY does not recognize and a
device prompt follows it, a later close line that names an outer host can leave
the remote profile active.

Use stdin mode for noninteractive output from files or pipes:

```sh
prismtty --profile cisco < show-tech.txt
journalctl -xe | prismtty --profile linux-unix
```

When PrismTTY reads a file or a pipe, highlighting pauses while the alternate
screen that full-screen programs such as `less` and `vim` draw on is active,
and resumes when that screen is closed or the terminal is reset. Output that
ends before then, such as a log cut off while a pager was open, stays
unhighlighted from the switch on.

A wrapped command runs in the current directory. Descriptors above 2 that the
caller passes (`ptty cmd 3<file`, `ptty diff <(a) <(b)`) reach the command as
they would without the wrapper. When stdin is not a terminal (a script, a CI
job, or `ptty make < /dev/null`), the command reads that stdin directly while
its output still goes through the PTY and is highlighted. If the
command cannot be found or run, PrismTTY exits with 127 or 126, as a shell does;
otherwise it exits with the command's own status. When the output goes to a
regular file (`ptty make > build.log`), or to a pipe while stdin is not a
terminal, the command's PTY starts with output processing off, so LF and CR LF
line ends are saved as the command wrote them; the command can change that
setting. With the keyboard on a terminal and the output in a pipe
(`ptty ssh host | tee log`) the PTY keeps its usual output settings, because
the pipe usually leads back to that terminal.
With stdin redirected and the output on a terminal (`ptty make </dev/null`),
the PTY keeps its usual settings too, so a recording of that terminal's output
holds an extra carriage return on each line (CR CR LF); on screen the lines
look the same. This is on purpose: the PTY ends each line with CR LF itself, so
the lines still start at the left edge if another program turns off the
terminal's output processing during the session.

A command killed by a signal is reported as 128 plus the signal number, as a
shell does, with one exception: when SIGINT (Ctrl-C) kills the command,
PrismTTY ends by SIGINT too, after restoring the terminal, so a loop typed at a
shell prompt stops after one Ctrl-C as it does without PrismTTY. In a shell
`$?` is still 130; a program that reads the wait status sees the signal
(Python's `subprocess` reports -2, `make` prints `Interrupt`). PrismTTY keeps
130 when it was started with SIGINT ignored or blocked. A script that runs with
the terminal on its stdin, and a zsh or dash pipeline where PrismTTY is not the
last command, still go on after one Ctrl-C, because those shells stop only when
they receive the Ctrl-C themselves and PrismTTY, holding the terminal in raw
mode, receives it instead.

When PrismTTY itself is sent a signal that ends it, such as SIGTERM, SIGHUP,
SIGINT or SIGQUIT, and the command is still running, PrismTTY passes the signal
on to the command's process group and, as before, kills what is left of that
group as soon as the command exits, or after two seconds. If the command exited
by itself (its wait status is an ordinary exit, even one that came near or
after those two seconds), PrismTTY, which gives the terminal back as soon as it
takes the signal, as before, then draws the output that reached it and exits
with the command's exit code, so a command that cleans up on SIGTERM and exits
0 gives 0. If the command was killed by a signal or could not be collected, or
that normal end does not complete within about four seconds of the signal, the
end is abrupt, as before: PrismTTY exits with 128 plus the signal it was sent,
and output not yet drawn can be lost. It ends by SIGINT instead when it was
sent SIGINT and the command died from it, so a command that dies from SIGINT
after PrismTTY was sent SIGTERM still gives 143. The four seconds count while
PrismTTY is not stopped; on macOS a PrismTTY that leads its terminal's session
also waits, as it exits, while output it wrote to that terminal is still
unread. The exit code describes the command PrismTTY started, not the rest of
its process group: 0 does not mean that every process in the group finished
cleanly. PrismTTY acts on the first such signal recorded before the final
pending-signal check in its shutdown handoff, including one recorded while the
session is ending; a later recorded one does not replace it. If none is
recorded by that check, PrismTTY gives the signals back to their previous
actions. Give-back and any reinstallation of PrismTTY's handlers each use one
`sigaction` per signal, so a signal during either transition can take its
previous action or reach PrismTTY's handler. A handler that entered earlier but
records such a signal only after the final check has no taker; that signal does
not change the session's otherwise determined result. After give-back completes
without reinstallation, later signals take their previous actions.

SIGUSR1 and SIGUSR2 sent to PrismTTY are passed to the command it started,
alone, while it runs, unless PrismTTY inherited them ignored or blocked, and do
not end PrismTTY themselves; like any signals of one kind they can coalesce,
and one that arrives after PrismTTY began to end the session is not passed on.
A worker that handles SIGUSR1 while the command PrismTTY started ignores it
gets nothing, and a shell that leaves SIGUSR1 at its default action dies from
it and ends the session; to reach a server started through a shell, start it
with `exec`, or send the signal to the server's own pid. A signal that PrismTTY
inherited as ignored or blocked, as `nohup` leaves SIGHUP and a script's
background job leaves SIGINT and SIGQUIT, is not passed on and does not end
PrismTTY: PrismTTY leaves it as its caller set it, as before. This holds for
SIGUSR1 and SIGUSR2 as for the signals that end it.

To page the output of a command that needs no keyboard, take the terminal off
its stdin:

```sh
ptty show-report </dev/null | less -R
```

PrismTTY then leaves the terminal to the pager. Without `</dev/null`, PrismTTY
and the pager both read the keyboard and both set the terminal's modes, and
quitting the pager while the command still runs can leave the terminal in the
pager's mode; `stty sane` puts it back.

On Linux 4.12+ the session normally ends once the wrapped command has exited and
its output is drawn, even if a background job holds the terminal (its later
output may not be shown); with an older kernel or no spare descriptors it waits
for the terminal's end. A line discipline other than N_TTY can lose output.

Important options:

- `-p, --profile <name>` forces one or more profiles.
- `--no-auto-detect` uses only `generic` unless profiles are forced.
- `--no-dynamic-profile` disables profile switching inside wrapped interactive shells.
- `-c, --config <file>` loads a ChromaTerm-compatible YAML file.
- `--strip-ansi` removes existing ANSI before PrismTTY styles output.
- `--sanitize` strips window-title, clipboard (OSC 52), and other OSC/DCS
  string escapes from program output while preserving normal CSI/SGR styling.
- `--show-profile` prints profile selections and transitions to stderr. With
  auto-detection on, a selection that stays `generic` is not reported, so a
  session that never leaves `generic` prints nothing.
- `--local-echo` locally echoes printable typed keys for no-echo device
  sessions. Do not use it at hidden prompts; it will echo typed secrets.
- `--no-minimal-reset` uses full SGR resets in interactive streams for terminal
  emulators that ignore minimal foreground/background resets. It can also be
  enabled with `PRISMTTY_NO_MINIMAL_RESET=1` or `PRISMTTY_NO_39_49_RESETS=1`.
- `--trace-io <file>` appends hex-encoded PTY input/output plus
  rendered-output diagnostics. Trace files include all keystrokes and output,
  including passwords typed during the session.
- `-R, --rgb` forces RGB color output.
- `--pcre` is accepted for ChromaTerm compatibility; PCRE2 is always used.
- `-b, --benchmark` prints per-rule timing and match-count data.
- `-r, --reload` asks running PrismTTY sessions to reload config.

Profile commands:

```sh
prismtty profiles list
prismtty profiles show cisco
prismtty profiles validate ~/.config/prismtty/profiles.d/my-vendor.yml
prismtty profiles test cisco fixtures/cisco.txt
```

## Configuration

PrismTTY loads built-in profiles first, then user rules. By default it checks:

- `~/.chromaterm.yml`
- `~/.chromaterm.yaml`
- `~/.config/chromaterm/chromaterm.yml`
- `~/.config/chromaterm/chromaterm.yaml`
- `~/.config/prismtty/config.yml`
- `~/.config/prismtty/config.yaml`
- `/etc/chromaterm/chromaterm.yml`
- `/etc/chromaterm/chromaterm.yaml`
- `~/.config/prismtty/profiles.d/*.yml`
- `~/.config/prismtty/profiles.d/*.yaml`

ChromaTerm-style rules are supported directly:

```yaml
rules:
  - description: IPv4
    regex: '\b192\.0\.2\.\d+\b'
    color: f#00ffff
```

Rules match bytes, not characters. A class like `[äöü]` is the set of bytes
those letters are made of, so it also colors other letters that share one of
those bytes, such as `é`, `ß` and `ñ`. To match characters, start the rule with
`(*UTF)`, as in `regex: '(*UTF)[äöü]'`. Since 1.3.12 such a rule keeps working
past bytes in the output that are not valid UTF-8.

Native profile files add metadata:

```yaml
profile:
  name: custom-router
  inherits: [generic]
  detection:
    - CustomOS
rules:
  - description: custom interface
    regex: '\bcust\d+/\d+\b'
    color: f#00ffff bold
```

Profiles under `~/.config/prismtty/profiles.d/` are first-class profiles: they
appear in `profiles list`, can be shown with `profiles show`, can inherit
built-in or other user profiles, and participate in auto-detection through their
`detection` hints.

## Built-In Profiles

- `generic`
- `juniper`
- `cisco`
- `arubacx`
- `versa`
- `arista`
- `fortinet`
- `palo-alto`
- `linux-unix`

These profiles are clean-room curated rule sets for prompts, interfaces,
addresses, protocol states, syslog severity, operational status, counters, and
common vendor terms.

## Runtime Reload

Long-running `ptty /bin/zsh` sessions register themselves in a small runtime
directory, by default `prismtty-<uid>` in the system temporary directory. That
is `$TMPDIR` when it is set, as it normally is on macOS, where it points to a
per-user directory under `/var/folders`; on Linux without `TMPDIR` it is `/tmp`.
Run this after editing `~/.chromaterm.yml` or files under `~/.config/prismtty/`:

```sh
prismtty --reload
```

The next output chunk in each running PrismTTY session reloads the active config.
Only sessions watching the same runtime directory see the reload request.
Set `PRISMTTY_RUNTIME_DIR` to override the runtime directory for tests or
isolated sessions. An existing directory keeps its mode; one that other users
can write to is refused, which only disables live reload for that session.
PrismTTY owns `pids.d` and `reload` inside that directory: each session sets
`pids.d` to mode 0700 and deletes every regular file in it whose name is not
the number of a running process. Do not point the variable at a directory that
has a `pids.d` or a `reload` of your own.

The name `prismtty-<uid>` is easy to predict. In a temporary directory that
other users can write to, such as `/tmp` on Linux without `TMPDIR` or a shared
directory named by `TMPDIR`, another local user who creates that name first
disables live reload for your sessions. Those sessions still show and
highlight their output, and each prints `prismtty: live reload disabled for
this session` and the reason. To avoid it, set `PRISMTTY_RUNTIME_DIR` to a
private directory, such as one in your home, or set `TMPDIR` to a private one.
The default temporary directory on macOS is a per-user one that only you can
write to, also when `TMPDIR` is unset, so it is not affected.

## Feedback Wanted

Testing feedback is especially useful for Cisco, Juniper, Fortinet, Palo Alto,
Arista, Aruba CX, Versa, Linux/Unix, and custom profile files for other vendors,
appliances, and terminal workflows.

## More Information

- Website: [prismtty.com](https://prismtty.com/)
- Source: [github.com/inxbit/prismtty](https://github.com/inxbit/prismtty)
- Releases: [github.com/inxbit/prismtty/releases](https://github.com/inxbit/prismtty/releases)
