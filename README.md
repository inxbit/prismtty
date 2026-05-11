# PrismTTY

PrismTTY is a fast terminal output highlighter focused on network devices and
Linux/Unix administration. It is intended as a ChromaTerm-style CLI wrapper with
network-focused built-in profiles.

Current version: `0.1.0`.

## Install

Build locally with:

```sh
cargo build --release
```

The project builds three binaries:

- `prismtty`
- `ptty`
- `ct`

Release packages can be built with:

```sh
scripts/package-release.sh darwin-aarch64
```

The package contains binaries, license/readme files, example profiles, shell
completions, and a `.tar.gz.sha256` checksum. A Homebrew formula template lives
under `packaging/homebrew/prismtty.rb`; update the release URLs/checksums after
publishing artifacts.

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

Interactive dynamic mode keeps built-in vendor selection conservative: after a
specific profile such as `generic, cisco` or `generic, juniper` is selected,
normal command output cannot add another vendor profile. Strong login banners can
still switch profiles, and typed nested remote commands can switch after the next
strong banner or repeated prompt.

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
