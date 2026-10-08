# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

- **Primary:** network engineers, NetOps staff and Unix/Linux sysadmins who live in a terminal on macOS or Linux and spend the day in SSH, telnet and console sessions on network devices (Cisco IOS / IOS XE / IOS XR / NX-OS / ASA, Juniper Junos, Fortinet FortiOS, Palo Alto PAN-OS, Arista EOS, Aruba CX, Versa) and Unix hosts.
- **Situation:** reading dense live output (`show ip interface brief`, `show logging`, `show interfaces terse`, `get system interface`, `journalctl`, `systemctl`) inside an interactive session, or from a saved dump or a pipe.
- **Job:** spot what matters (a down interface, a syslog severity, an address, a protocol state, a counter) without reading every byte.
- **Secondary audiences the product serves:** ChromaTerm users (PrismTTY reads their config and ships a `ct` command), Rust developers using the library on docs.rs, and operators who can send real-world feedback on a vendor profile.

## Product Purpose

PrismTTY makes dense SSH, network-device and Unix/Linux terminal output easier to scan in real time by coloring output that is already flowing through the terminal. It changes nothing about how the operator works: start one wrapped shell (`ptty /bin/zsh`) from the terminal profile and keep using ssh, telnet and console commands inside it. Highlighting follows the session as it hops between devices. The same engine colors files and pipes in stdin mode (`prismtty --profile cisco < show-tech.txt`, `journalctl -xe | prismtty --profile linux-unix`). It also ships as a Rust library.

Success (owner-confirmed 2026-10-07): an operator installs it and keeps it as their default shell wrapper; operators send profile feedback through GitHub issues (the one explicit ask the project makes). No metrics are collected.

## Positioning

Self-description: "a fast ChromaTerm-style CLI wrapper with network-focused built-in profiles"; "a live terminal-output highlighter for shells, SSH sessions, pipes, and logs". Current site line: "Readable output, live in your terminal."

The mechanism a neighbor could not truthfully copy is the combination:

1. **One wrapped shell follows the device context.** A typed remote-jump command (ssh, mosh, telnet, screen, cu, minicom, picocom) arms a switch; the next login banner or prompt pushes the matching vendor profile onto a bounded stack (8 deep), locked so ordinary output cannot churn it; close markers ("Connection to X closed", "Connection closed by foreign host", "logout", screen/minicom/picocom exits) pop back to the previous profile. Failed connection attempts neither push nor pop.
2. **Network-first, clean-room built-in profiles** with data-driven vendor detection.
3. **Prompt-echo-aware interactive rendering**: Tab / `?` completion and history-recall redraws stay visible; only the child's own bytes are ever emitted (ADR 0001).
4. **ChromaTerm-compatible**: reads existing `~/.chromaterm.yml`, ships `ct`, accepts `--pcre`.
5. **Hardened for hostile remote output**: opt-in `--sanitize`, per-rule PCRE2 match and depth limits, bounded buffers.

Guardrail: the repo holds no comparison against any other tool. Describe what PrismTTY does; never claim what other tools lack or how fast they are next to it.

## Operating Context

- Runs locally in the user's terminal emulator, macOS and Linux only (Windows: WSL). Prebuilt binaries for darwin-aarch64, darwin-x86_64, linux-x86_64.
- Install: `brew install inxbit/tap/prismtty` (prismtty, ptty, ct, completions, example profiles, PCRE2); `cargo install prismtty` (Rust 1.85+, PCRE2, pkg-config); GitHub release tarballs with `.sha256` and attestations.
- A session is one wrapped shell launched from the terminal profile; signals are forwarded and exit codes preserved. `prismtty --reload` hot-reloads running sessions after config edits.
- Config: `~/.config/prismtty/config.yml` + `profiles.d/` user profiles; ChromaTerm YAML auto-loaded from its usual paths.
- Website: static GitHub Pages site from `docs/` on `main`, no build step, strict meta CSP, self-hosted fonts, no third-party hosts. `tests/site_pages.test.mjs` gates Pages, CI and releases. `scripts/privacy-scan.sh` scans every tracked file.

## Capabilities and Constraints

**Capabilities**
- Commands `prismtty`, `ptty`, `ct`: three names for one identical binary.
- Nine built-in profiles: generic, linux-unix, cisco, juniper, fortinet, palo-alto, arista, arubacx, versa (arista inherits cisco; others inherit generic). They color prompts, interfaces, addresses (IPv4, IPv6, MAC), protocol and operational state, syslog severity, counters and vendor terms.
- Dynamic profile switching with push/pop nesting; `--show-profile`, `--no-dynamic-profile`.
- Stdin/pipe mode, forced profiles (`-p`, repeatable), `--no-auto-detect`.
- User profiles in `profiles.d` (inherit, detection hints, list/show/validate/test tooling).
- PCRE2 engine with per-rule limits; `-b/--benchmark`; `--trace-io`; `--local-echo`; `--strip-ansi`; `-R` truecolor; `--sanitize`.
- Shell completions for zsh, bash, fish.
- No network code in the binary; never logs in to devices, collects inventory, pushes config or stores operational data.

**Non-goals:** not an NMS, configuration tool, source of truth, SIEM, log platform or inventory system.

**Claim constraints**
- Privacy: "no network dependencies, no telemetry, nothing stored by default". `--trace-io` and `--local-echo` are opt-in exceptions.
- `--sanitize` is opt-in; never present escape filtering as default.
- PCRE2 limits are defense in depth, not immunity.
- Interactive fidelity: "designed to preserve" echo, line editing and completion, not "never loses".
- Vendor coverage is "feedback wanted", never complete. Every fixture is synthetic.
- Platforms: macOS and Linux only.
- Demo data: synthetic only, documentation-range addresses (192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24, 2001:db8::/32), invented hosts (`example.net` by convention), no real captures.
- A demo presented as real output must be real PrismTTY output; anything else is labeled illustration.
- The string `show-tech.txt | prismtty` must not appear in the site or READMEs.

**Terminology:** PrismTTY (prose, TTY in caps); `prismtty` (crate, binary, repo); `ptty`; `ct`. Wrapped shell, stdin mode, profile, built-in profile, user profile, dynamic profile switching, remote-jump command, close marker, profile stack, clean-room profiles, runtime reload.

## Brand Commitments

- Name and wordmark: "Prism" + "TTY".
- Logo: keep the current prism mark and PrismTTY lockup, same construction, redrawn crisp (owner, 2026-10-07). Assets: `docs/assets/prismtty-mark.svg` (favicon), `prismtty-lockup.svg`, `prismtty-logo.svg`.
- Hotlinked paths that must keep existing: `docs/assets/prismtty-logo.svg`, `prismtty-terminal-demo.svg`, `prismtty-terminal-preview.svg`, `prismtty-profile-switching.svg` (published crates.io READMEs load them from prismtty.com), and the social card URL `https://prismtty.com/assets/prismtty-social-card.png`.
- Voice: plain, factual, scope-honest, operator-facing. No staccato marketing copy.
- Footer claims in use: "clean-room" profiles, "no telemetry".
- Demos show real PrismTTY output in its real default colors, generated from the synthetic fixtures (owner, 2026-10-07). Today's JS imitation highlighter is retired.

## Evidence on Hand

- 28 releases since 2026-05-12 (v0.2.1) to 1.2.5 (2026-08-20), all in CHANGELOG.md; MIT licensed; crates.io, docs.rs, Homebrew tap, GitHub Releases with checksums and attestations.
- Maintainer-measured before/after deltas (own hardware, not head-to-head): piped highlighting of a synthetic 20k-line router dump 0.57 s to 0.14 s; interactive throughput 1.1 to 4.4 MiB/s; release binary 2.28 MB to 1.76 MB (CHANGELOG 1.2.2).
- 485 Rust tests, 35 node tests; CI on macOS + Ubuntu with cargo-audit, cargo-deny, MSRV, privacy scan.
- Synthetic fixtures: `fixtures/cisco.txt`, `juniper.txt`, `linux-unix.txt`, and 10 replay fixtures across 8 profiles in `fixtures/replay/` with an expectations contract.
- **Real output is producible:** `prismtty -R -p <profile> < fixtures/<file>` (run hermetically: empty HOME/XDG, `PRISMTTY_RUNTIME_DIR` set) emits truecolor ANSI that converts to HTML; replay renders pass the expectations contract. Real default colors are the profiles' own: interfaces `#0099ff`, addresses `#00ffff`, good state `#00ff00`, bad state bold `#ff0000`, prompts `#ffffff`, accents `#ff00ff`. Real quirks show (Cisco `OK?` header and systemd `Loaded:` colored as good). Auto-detect picks the right profile for the three primary fixtures.
- ADR `docs/adr/0001-prompt-echo-aware-idle-flush.md` (published raw on the site).
- **Absences (never fabricate):** testimonials, quotes, case studies; user, download, star or install counts; head-to-head benchmarks; "used by" logos; vendor affiliation; real device captures; Windows or linux-aarch64 binaries; blog, docs site, roadmap, pricing.

## Product Principles

1. **Change nothing about how the operator works.** One wrapped shell, usual commands, usual configs and ChromaTerm habits.
2. **Highlight only; never manage.** "Live terminal highlighting, not device management" is a product boundary.
3. **Claim exactly what is true by default.** Default vs opt-in kept apart; measurements carry provenance; coverage is "feedback wanted".
4. **Proof is real output over synthetic data.** Real PrismTTY runs over synthetic fixtures with documentation-range addresses; never real captures.
5. **Self-contained by construction.** No network code in the binary, no third-party hosts on the site; loosening the CSP is a deliberate owner decision (2026-10-07: `media-src 'self'` approved for self-hosted video, nothing else).

## Accessibility & Inclusion

- Site: text tokens reach at least 4.5:1 on every surface (enforced by the site test); keyboard-operable controls (the profile tabs use a roving-tabindex tablist); reduced-motion paths for every animated demo; 44 px touch targets and a 12 px terminal-type floor on small screens.
- Color is the product's core signal; color-vision-deficiency handling in profiles is undecided.
