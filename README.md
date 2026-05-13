# PrismTTY

<p align="center">
  <img src=".github/assets/prismtty-logo.svg?v=20260513b" alt="PrismTTY" width="760">
</p>

<p align="center">
  <a href="https://prismtty.com/"><img alt="website" src="https://img.shields.io/badge/website-prismtty-22d3ee?style=flat-square"></a>
  <a href="https://crates.io/crates/prismtty"><img alt="crates.io" src="https://img.shields.io/crates/v/prismtty?style=flat-square"></a>
  <a href="https://github.com/inxbit/prismtty/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/inxbit/prismtty/ci.yml?branch=main&amp;style=flat-square"></a>
  <a href="https://github.com/inxbit/prismtty/releases"><img alt="release" src="https://img.shields.io/github/v/release/inxbit/prismtty?style=flat-square"></a>
  <a href="LICENSE"><img alt="license" src="https://img.shields.io/github/license/inxbit/prismtty?style=flat-square"></a>
</p>

PrismTTY is a fast terminal output highlighter focused on network devices and
Linux/Unix administration. It is intended as a ChromaTerm-style CLI wrapper with
network-focused built-in profiles.

Website: [prismtty.com](https://prismtty.com/).

Current version: `0.2.1`.

## Preview

Synthetic terminal previews using documentation-only hostnames and addresses:

![Highlighted PrismTTY terminal output](.github/assets/prismtty-terminal-preview.svg?v=20260513)

![Dynamic PrismTTY profile switching](.github/assets/prismtty-profile-switching.svg?v=20260513b)

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
[v0.2.1 release page](https://github.com/inxbit/prismtty/releases/tag/v0.2.1).

Each release archive contains the binaries, license/readme files, example
profiles, shell completions, and a `.tar.gz.sha256` checksum.

### Build Locally

Build from a checkout with:

```sh
cargo build --release
```

The project builds these user-facing binaries:

- `prismtty`
- `ptty`
- `ct`

Release packages can be built with:

```sh
scripts/package-release.sh darwin-aarch64
```

The release workflow generates a Homebrew formula from the actual release
checksums and uploads it as `prismtty.rb` with the release artifacts. The
published tap formula lives in
[inxbit/homebrew-tap](https://github.com/inxbit/homebrew-tap).

## Usage

```sh
ptty /bin/zsh
ptty ssh router.example.net
show-tech.txt | prismtty --profile cisco
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
then keeps the selected remote profile locked for the session. Normal command
output such as interface descriptions cannot churn the profile in the middle of
the session. Nested remote sessions are still supported: a typed remote-jump
command arms the next strong banner or repeated prompt to push a new profile, and
connection-close markers pop back to the previous profile.

Interactive mode is designed to preserve shell line editing and typed command
echo, including zsh prompts that use decorative Unicode prompt markers such as
`╰─○`, `❯`, or `➜`.

Use pipe mode for noninteractive output:

```sh
show-tech.txt | prismtty --profile cisco
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
appear in `profiles list`, can be shown with `profiles show`, can inherit built-in
or other user profiles, and participate in auto-detection through their
`detection` hints.

Built-in profiles are bundled data-driven profile files compiled into the binary.
Their private runtime detection metadata is intentionally not part of the public
user profile schema yet. If a user profile under `profiles.d` includes
`profile.runtime`, `profiles validate` and normal startup loading reject it as a
reserved field.

## Reload

Long-running `ptty /bin/zsh` sessions register themselves in a small runtime
directory under `/tmp` by default. Run this after editing `~/.chromaterm.yml` or
files under `~/.config/prismtty/`:

```sh
prismtty --reload
```

The next output chunk in each running PrismTTY session reloads the active config.
Set `PRISMTTY_RUNTIME_DIR` to override the runtime directory, which is useful for
tests or isolated sessions.

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

The built-in rule sets and detection hints live as bundled profile data rather
than hardcoded vendor branches. Adding or adjusting a built-in profile should
start from the bundled profile file and its tests, while user profiles continue
to use the public schema shown above. Bundled runtime metadata can also include
private `negative_signals` that block only startup prompt matcher detection; weak
`detection` hints, strong banner signals, and runtime prompt transitions are not
blocked by those exclusions.

Interactive dynamic mode keeps built-in vendor selection conservative: after a
specific profile such as `generic, cisco` or `generic, juniper` is selected,
normal command output cannot add another vendor profile. Strong login banners can
still switch profiles, and typed nested remote commands can switch after the next
strong banner or repeated prompt. Fortinet is intentionally more permissive only
from a local-shell baseline: a single `hostname #` Fortinet prompt can promote to
`generic, fortinet`, which supports local shell wrappers such as `fw` that do not
show a FortiOS banner or an explicit `ssh` command. Other prompt-only vendor
switches still require repeated evidence or a typed remote-jump command.

When PrismTTY is launched as an iTerm profile command, it strips iTerm's native
shell-integration variables from the child shell to avoid nested integration
marks. The stripped values are preserved under `PRISMTTY_PARENT_*` names for
dotfiles that need session or profile context. For example:

```sh
_iterm_session_id="${ITERM_SESSION_ID:-${PRISMTTY_PARENT_ITERM_SESSION_ID:-}}"

if [[ $_iterm_session_id =~ w[0-9]+t0p0 ]]; then
    fastfetch
fi
```

## Development Notes

The CLI entry point is intentionally split into focused internal modules:

- `src/cli.rs` keeps `run()`, action dispatch, and CLI error handling.
- `src/cli/args.rs` owns the clap-backed parser and parser contract tests.
- `src/cli/profile_selection.rs` loads profile stores, user config, color mode,
  and profile reporting.
- `src/cli/pty.rs` owns PTY command execution, raw mode, stdin forwarding, local
  echo, the iTerm environment guard, and resize polling.
- `src/cli/stream.rs` owns streaming highlight orchestration, dynamic profile
  observation, reload handling, benchmarks, and `HighlightSession`.
- `src/cli/runtime.rs` owns runtime registration and reload markers.
- `src/cli/trace.rs` owns `--trace-io` formatting.

Keep these modules internal to the CLI tree. Parser contract tests intentionally
preserve current behavior, including `--pcre` as a no-op compatibility flag, so
future parser migrations can be checked without changing user-facing semantics.
The data-driven profile refactor did not keep a temporary old/new detector
compatibility module; instead, the current safety net is the runtime transition
tests plus byte-for-byte Cisco and Juniper streaming snapshots.

## Benchmark

Run the built-in throughput benchmark example:

```sh
cargo run --release --example throughput
```

The example runs router dump, syslog, and mixed-ANSI samples. Runtime benchmark
mode is available in normal use:

```sh
show-tech.txt | prismtty --benchmark --profile cisco >/dev/null
```

## Replay Fixtures

Replay fixtures under `fixtures/replay/` are synthetic. They exist to protect
profile detection and streaming coloring behavior across chunk boundaries without
checking private device output into the repository.

Run the replay suite with:

```sh
cargo test --test replay
```

When adding a new replay case, use invented hostnames, documentation-range IP
addresses, and minimal command output that preserves only the token shape needed
for the rule under test. Do not copy real terminal captures into this directory.

## License

MIT. See [LICENSE](LICENSE).
