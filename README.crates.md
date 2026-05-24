# PrismTTY

<p align="center">
  <img src="https://prismtty.com/assets/prismtty-logo.svg" alt="PrismTTY" width="760">
</p>

<p align="center">
  <a href="https://prismtty.com/"><img alt="website" src="https://img.shields.io/badge/website-prismtty-22d3ee?style=flat-square"></a>
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

![Animated PrismTTY terminal demo](https://prismtty.com/assets/prismtty-terminal-demo.svg)

## Preview

Synthetic terminal previews using documentation-only hostnames and addresses:

![Highlighted PrismTTY terminal output](https://prismtty.com/assets/prismtty-terminal-preview.svg)

![Dynamic PrismTTY profile switching](https://prismtty.com/assets/prismtty-profile-switching.svg)

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

### Cargo

Install from crates.io:

```sh
cargo install prismtty
```

Cargo builds PrismTTY from source and installs the command binaries. You need
Rust 1.85 or newer plus PCRE2 and `pkg-config` available on your system.

For `cargo install` on macOS, install the native build dependencies first:

```sh
brew install pcre2 pkg-config
```

For `cargo install` on Debian/Ubuntu, install:

```sh
sudo apt-get install libpcre2-dev pkg-config
```

### GitHub Release

Prebuilt release archives and checksums are available on the
[GitHub releases page](https://github.com/inxbit/prismtty/releases).

Each release archive contains the binaries, license/readme files, example
profiles, shell completions, and a `.tar.gz.sha256` checksum.

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

Use stdin mode for noninteractive output from files or pipes:

```sh
prismtty --profile cisco < show-tech.txt
journalctl -xe | prismtty --profile linux-unix
```

Important options:

- `-p, --profile <name>` forces one or more profiles.
- `--no-auto-detect` uses only `generic` unless profiles are forced.
- `--no-dynamic-profile` disables profile switching inside wrapped interactive shells.
- `-c, --config <file>` loads a ChromaTerm-compatible YAML file.
- `--strip-ansi` removes existing ANSI before PrismTTY styles output.
- `--show-profile` prints profile selections and transitions to stderr.
- `--local-echo` locally echoes printable typed keys for no-echo device sessions.
- `--no-minimal-reset` uses full SGR resets in interactive streams for terminal
  emulators that ignore minimal foreground/background resets. It can also be
  enabled with `PRISMTTY_NO_MINIMAL_RESET=1` or `PRISMTTY_NO_39_49_RESETS=1`.
- `--trace-io <file>` appends hex-encoded PTY input/output plus rendered-output diagnostics.
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
directory under `/tmp` by default. Run this after editing `~/.chromaterm.yml` or
files under `~/.config/prismtty/`:

```sh
prismtty --reload
```

The next output chunk in each running PrismTTY session reloads the active config.
Set `PRISMTTY_RUNTIME_DIR` to override the runtime directory for tests or
isolated sessions.

## Feedback Wanted

Testing feedback is especially useful for Cisco, Juniper, Fortinet, Palo Alto,
Arista, Aruba CX, Versa, Linux/Unix, and custom profile files for other vendors,
appliances, and terminal workflows.

## More Information

- Website: [prismtty.com](https://prismtty.com/)
- Source: [github.com/inxbit/prismtty](https://github.com/inxbit/prismtty)
- Releases: [github.com/inxbit/prismtty/releases](https://github.com/inxbit/prismtty/releases)
