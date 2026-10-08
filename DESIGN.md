---
name: PrismTTY
description: Readable network output, live in your terminal. One session crosses a glass prism and leaves in real PrismTTY colors.
colors:
  ground: "#0a0b0d"
  text: "#f2f3f5"
  text-2: "#c4c7ce"
  muted: "#8b9099"
  dim: "#7b8087"
  hairline: "#23252a"
  chip: "#0e1014"
  chip-line: "#2a2d35"
  ghost-line: "#3a414c"
  term-surface: "#0c0e11"
  selected-surface: "#15181d"
  output-interface: "#0099ff"
  output-prompt-alt: "#00bfff"
  output-up: "#00ff00"
  output-port: "#00ffc0"
  output-address: "#00ffff"
  output-severity: "#65d7fd"
  output-down: "#ff0000"
  output-accent: "#ff00ff"
  output-prompt: "#ffffff"
typography:
  display:
    fontFamily: "Vend Sans, ui-sans-serif, system-ui, -apple-system, Segoe UI, sans-serif"
    fontSize: "calc(75 * var(--u))"
    fontWeight: 700
    lineHeight: "calc(76 * var(--u))"
    letterSpacing: "-0.017em"
  headline:
    fontFamily: "Vend Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "max(30px, calc(54 * var(--u)))"
    fontWeight: 700
    lineHeight: 1.08
    letterSpacing: "-0.025em"
  title:
    fontFamily: "Vend Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "max(17px, calc(22 * var(--u)))"
    fontWeight: 500
    lineHeight: "calc(34 * var(--u))"
  lede:
    fontFamily: "Vend Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "max(17px, calc(21 * var(--u)))"
    fontWeight: 300
    lineHeight: 1.55
  body:
    fontFamily: "Vend Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "max(15px, calc(18 * var(--u)))"
    fontWeight: 400
    lineHeight: "calc(27 * var(--u))"
  label:
    fontFamily: "Vend Sans, ui-sans-serif, system-ui, sans-serif"
    fontSize: "max(14px, calc(17 * var(--u)))"
    fontWeight: 400
    lineHeight: "max(20px, calc(28 * var(--u)))"
  mono-output:
    fontFamily: "Oxygen Mono, ui-monospace, SF Mono, Menlo, Consolas, monospace"
    fontSize: "max(13px, calc(17 * var(--u)))"
    fontWeight: 400
    lineHeight: 1.65
  mono-hero-output:
    fontFamily: "Oxygen Mono, ui-monospace, SF Mono, Menlo, Consolas, monospace"
    fontSize: "max(14.5px, calc(19 * var(--u)))"
    fontWeight: 400
    lineHeight: "max(22px, calc(29 * var(--u)))"
  mono-command:
    fontFamily: "Oxygen Mono, ui-monospace, SF Mono, Menlo, Consolas, monospace"
    fontSize: "max(15px, calc(18 * var(--u)))"
    fontWeight: 400
    letterSpacing: "-0.03em"
  mono-label:
    fontFamily: "Oxygen Mono, ui-monospace, SF Mono, Menlo, Consolas, monospace"
    fontSize: "max(12px, calc(15 * var(--u)))"
    fontWeight: 400
rounded:
  focus: "4px"
  control: "calc(8 * var(--u))"
  row: "max(8px, calc(10 * var(--u)))"
  term: "max(10px, calc(14 * var(--u)))"
  pill: "999px"
spacing:
  comp-unit: "calc(min(100vw, 1672px) / 1672)"
  frozen-unit: "0.765px"
  frame-margin: "calc(70 * var(--u))"
  phone-gutter: "20px"
  section-pad: "max(56px, calc(100 * var(--u)))"
  head-gap: "max(28px, calc(44 * var(--u)))"
  lede-gap: "max(14px, calc(20 * var(--u)))"
  pane-pad: "max(16px, calc(26 * var(--u))) max(16px, calc(28 * var(--u)))"
  touch-min: "44px"
components:
  install-chip:
    backgroundColor: "{colors.chip}"
    textColor: "{colors.text-2}"
    rounded: "{rounded.control}"
    height: "max(44px, calc(56 * var(--u)))"
    width: "calc(474 * var(--u))"
  button-ghost:
    backgroundColor: "transparent"
    textColor: "#dde3e9"
    rounded: "{rounded.control}"
    height: "max(44px, calc(56 * var(--u)))"
  button-ghost-hover:
    textColor: "{colors.text}"
  button-solid:
    backgroundColor: "{colors.text}"
    textColor: "{colors.ground}"
    rounded: "{rounded.control}"
    height: "max(48px, calc(56 * var(--u)))"
    padding: "0 max(18px, calc(24 * var(--u)))"
  button-solid-hover:
    backgroundColor: "#ffffff"
    textColor: "{colors.ground}"
  tab:
    backgroundColor: "transparent"
    textColor: "{colors.muted}"
    typography: "{typography.mono-label}"
    rounded: "{rounded.pill}"
    height: "44px"
    padding: "0 max(14px, calc(18 * var(--u)))"
  tab-selected:
    backgroundColor: "{colors.selected-surface}"
    textColor: "{colors.text}"
  term:
    backgroundColor: "{colors.term-surface}"
    textColor: "{colors.text-2}"
    typography: "{typography.mono-output}"
    rounded: "{rounded.term}"
  install-row:
    backgroundColor: "{colors.chip}"
    textColor: "{colors.text-2}"
    typography: "{typography.mono-command}"
    rounded: "{rounded.row}"
    height: "max(52px, calc(60 * var(--u)))"
---

# Design System: PrismTTY

## Overview

**Creative North Star: "The Prism at the Seam"**

The site is a dark optical bench with one instrument on it. A single Cisco session crosses the first viewport as raw gray text, passes through a glass prism and leaves in PrismTTY's real colors. Everything else on the page is set in near-black, off-white and a short ladder of grays, so that the only saturated light anywhere is the product's own output. The prism is the single optical object; the crystals elsewhere are the same glass, out of focus.

The world is terminal-native and restrained: one refined variable sans (Vend Sans) for display and UI, one clean mono (Oxygen Mono) for every command and every line of output. Surfaces are flat and separated by 1px hairlines, not shadows. Controls are quiet outlines and pills; there is exactly one solid button per page. Motion is a one-time demonstration (the footage plays once and stops on the wide spectrum, each raw line passes into the prism once) followed by a slow steady light, never a loop that asks for attention.

The desktop hero is a measured frame: every length is a comp pixel of the approved 1672 px comp times `--u`, with px floors for legibility. Below 1280 px the frame lets go and flows as a column.

**Key Characteristics:**
- Near-black ground, gray type ladder, hairline structure.
- Saturated color only where PrismTTY itself produced it.
- One optical object (the prism), one light layer, crystals in the empty side of each section.
- Comp-pixel geometry on desktop, frozen unit and flow below 1280 px.
- Every raster carries its provenance.

## Colors

A monochrome neutral ladder on near-black, broken only by the nine default colors PrismTTY's profiles actually emit.

### Primary
The primary palette is real output, not brand color. These values live between the `output-colors:start` / `output-colors:end` markers in `docs/site.css` as `.c-rrggbb` classes, are generated by `scripts/site-output.mjs` from `fixtures/site/*.txt`, and are guarded by `tests/site_pages.test.mjs` and `tests/site_output.rs`. Never hand-edit them and never add one that the binary did not produce.
- **Interface Blue** (output-interface): interface names (Gi1/0/1, ge-0/0/0, Vlan1191).
- **Junos Prompt Sky** (output-prompt-alt): `user@host>` prompts on Juniper, Palo Alto and Versa.
- **State Up Green** (output-up): up, OK, ACTIVE, accepted.
- **Port Mint** (output-port): service ports after an address (`:22`).
- **Address Cyan** (output-address): IPv4, IPv6, MAC and prefixes.
- **Severity Ice** (output-severity): syslog severity such as LOG_INFO.
- **State Down Red** (output-down): down, failed, root; always bold.
- **Vendor Magenta** (output-accent): vendor terms (vsys, MLAG, ArubaOS-CX).
- **Prompt White** (output-prompt): `host#` prompts.

### Neutral
- **Bench Black** (ground): page ground, theme-color, the color behind every plate.
- **Paper White** (text): headings, active controls, the solid button fill.
- **Output Gray** (text-2): uncolored output, list items, inline code, badge text.
- **Muted Steel** (muted): ledes, tab and copy-icon rest state.
- **Dim Steel** (dim): captions, notes, install labels, `$` prompts, footer facts.
- **Hairline** (hairline): every section border, rule and divider.
- **Chip Black** (chip) with **Chip Line** (chip-line): install chip and install rows.
- **Ghost Line** (ghost-line): ghost button border, selected tab and active hop chip border, link underline.
- **Terminal Surface** (term-surface): the inside of terminal windows, a step above ground.
- **Selected Surface** (selected-surface): selected tab, pressed ghost button and tab.

### Named Rules
**The Real Output Rule.** No saturated color exists on the site unless PrismTTY produced it from a fixture. The only exceptions are the logo mark, which keeps its own fixed construction (a white-to-cyan-to-violet gradient prism stroke around a terminal with a cyan chevron and a lime cursor), and two UI borrowings from output: the keyboard focus ring (Address Cyan, 2px) and text selection (Interface Blue at 33% alpha). Add no others, and never reuse the mark's colors elsewhere.

**The Gray Ladder Rule.** UI text steps down Paper White, Output Gray, Muted Steel, Dim Steel. Every step holds 4.5:1 on its surface (enforced by the site test); a new gray must clear the same bar before it ships.

## Typography

**Display Font:** Vend Sans, variable 300-700, self-hosted (with ui-sans-serif, system-ui)
**Body Font:** Vend Sans
**Label/Mono Font:** Oxygen Mono 400, self-hosted (with ui-monospace, SF Mono, Menlo, Consolas)

**Character:** A precise, slightly technical grotesque that tightens at display size, paired with a calm mono that makes every command and every line of output read as a real terminal.

### Hierarchy
- **Display** (700, 75u on 76u, -0.017em): the two-line hero headline only. Below 1280 px: clamp(38px, 9.4vw, 66px) on 1.04.
- **Headline** (700, max(30px, 54u), 1.08, -0.025em, balanced): section headings and the 404 title.
- **Title** (500, max(17px, 22u)): fact titles and scope column heads.
- **Lede** (300, max(17px, 21u), 1.55, max 62ch): the paragraph under each headline, in Muted Steel. Below 1280 px ledes step up to 400 for legibility.
- **Body** (400, max(15px, 18u), 27u): fact bodies and scope prose (max 58ch).
- **Label** (400, max(14px, 17u)): captions, install labels, footer facts.
- **Mono output** (400, max(13px, 17u), 1.65): terminal panes; hero output at max(14.5px, 19u) on 29u. Bold only where the output is bold.
- **Mono command** (400, max(15px, 18u), -0.03em): install commands.
- **Mono label** (400, max(12px, 15u)): terminal title bars, tabs, hop stack, scope and vendor lists.

### Named Rules
**The Two Faces Rule.** Vend Sans speaks, Oxygen Mono shows. Anything a user would type or a device would print is mono; nothing else is.

**The Terminal Floor Rule.** Terminal type never drops below 12px; on a 390 px phone the hero output sets 12px at -0.03em so 51 columns fit.

## Layout

The desktop hero is a measured frame, max 1672 px wide and 941u tall, with every length expressed as comp pixels times `--u` (one comp pixel = min(100vw, 1672px) / 1672) and px floors on anything read. Nav, headline, lede, actions, output panes, prism, caption, rule and the three-column fact trio are absolutely placed on that frame: raw output from 30u, prism media at 500u (560u by 332u), highlighted output from 1063u, rule at 762u, facts at 792u in columns of 482u, 532u and the rest.

Below 1280 px the hero leaves its frame and flows as a single column with 20 px gutters, and `--u` freezes at 0.765px so every comp-pixel length keeps a fixed size. Between 700 and 1279 px the fact trio stays three columns; below 700 px it stacks with hairlines between items.

Sections below the hero share the hero's grid: a wrap of min(1532u, 100% - 40px), section padding of max(56px, 100u) top and bottom, a 1px hairline on top, heads capped at max(320px, 980u). Breakpoints in use: 1279, 700, 600, 520, 400, 360 px.

### Named Rules
**The Comp Pixel Rule.** On desktop, new hero geometry is written as `calc(N * var(--u))` with N read off the comp, plus a px floor wherever text or a touch target could get too small. Never mix raw px into the measured frame.

**The Empty Side Rule.** Each section's crystal plate sits in the section's empty side and fades out before the section edge. Crystals never sit under running text, and the scope section carries none.

## Elevation & Depth

The system is flat. There are no box-shadows, text-shadows or blur filters. Depth comes from three things only: tonal steps (ground, terminal surface, chip, selected surface), 1px hairlines, and photographic light. The light is screen-blended raster plates (the hero light plate, the prism, the off-focus crystals at 0.5-0.6 opacity) shaped by gradient masks so they dissolve into the ground with no visible edge.

### Named Rules
**The No Glow Rule.** No glow on UI: no shadows, no blurred halos, no gradient text, no gradient fills on surfaces. Gradients appear only as mask-image on plates and panes, as the beam fill in the light layer, and inside the logo mark.

**The One Object Rule.** The prism is the only optical object in focus. Other glass on the page is out of focus and behind content.

## Shapes

Gently rounded rectangles throughout: controls at 8u (8px when the hero flows), install rows at max(8px, 10u), terminal windows at max(10px, 14u). Selectors and status markers are full pills (999px): tabs and the terminal badge. Hop-stack chips use 8px. Everything is outlined with 1px lines rather than filled; the focus ring is 2px with a 3px offset and 4px corners. Scrolling panes fade their cut right edge with a 48px mask until scrolled to the end.

## Components

### Buttons
Quiet outlines, one solid voice per page.
- **Shape:** gently rounded (8u, 8px below 1280 px), height max(44px, 56u).
- **Ghost:** transparent, 1px Ghost Line border, light gray label, optional 22u GitHub mark. Hover lifts the border to #59616d and the label to Paper White; pressed fills Selected Surface. Transitions are 0.15s ease on color, border and background.
- **Solid:** Paper White fill, Bench Black label, weight 500, height max(48px, 56u); hover goes to pure white. On the home page it is the feedback call to action; on 404 it is the way home. One per page.

### Install Chip and Copy
The hero's primary action. A Chip Black field with a Chip Line border holds the mono command (Dim Steel `$` prompt, aria-hidden) and a copy button divided by a 1px line. The copy icon is a two-rect stroke SVG that swaps to a check for 1.6 s on success; a status line announces "Copied", and if the clipboard is refused the command (minus its `$`) is selected instead. Install rows repeat the pattern at section scale with a label column that hides below 520 px.

### Tabs
Mono pill tabs, min 44 px tall, 1px #23262c border, Muted Steel at rest. Selected: Ghost Line border, Selected Surface fill, Paper White label. Roving tabindex with arrow keys, Home and End; tabs wrap below 600 px.

### Terminal Window
Terminal Surface inside a 1px hairline frame at max(10px, 14u) radius. A mono title bar (ptty /bin/zsh) carries a pill badge naming the active profile stack. Panes are mono output on 1.65 line height, scroll sideways with thin dark scrollbars and the right-edge fade cue. Notes below sit in Dim Steel at max 76ch.

### Hop Stack
The profile stack beside each hop: mono chips with a 1px #23262c border and 8px corners; the active profile gets a Ghost Line border and Paper White; a popped profile is struck through in Dim Steel; push and pop are + and − with screen-reader words.

### Navigation
Logo variant C (the prism mark with a terminal inside) at 60u plus the PrismTTY wordmark at 27u, weight 650. Links at max(15px, 19u) in light gray, hover to Paper White. Below 1280 px the nav becomes a flex row with 15px links and 44px hit areas; the wordmark hides below 400 px and README hides below 360 px. Footer and install links underline in Ghost Line at 4px offset and brighten on hover.

### Fact Trio
Three facts on the hero's bottom edge, each a 44u stroke icon (2px, currentColor, custom SVG) beside a title and body, divided by vertical hairlines; stacked with horizontal hairlines on phones.

### The Seam (signature)
Raw output left, the prism in the middle, the same output through PrismTTY right, captioned "Real PrismTTY output, cisco profile." The still prism plate is the default. With motion allowed, the approved Higgsfield image-to-video take (take 1 of the approved prism) plays once, eases per frame to a stop on the wide spectrum, and the light layer fades in over 1.6s (cubic-bezier(0.16, 1, 0.3, 1)): a steady white beam flowing left to right (1.5s linear loop, screen-blended) and canvas dust that only shows inside the beam and the spectrum fan. On load each raw line flashes as it passes into the prism and its highlighted twin wipes in 0.32s later, 0.18s apart. Reduced motion, data saver, a video error or a stall keep the still plate. The light layer is hidden below 1280 px. The hero light plate and crystals drift slowly (26s and 34s, alternate) when motion is allowed.

## Do's and Don'ts

### Do:
- **Do** take every saturated color from real PrismTTY output: add a fixture under `fixtures/site/`, regenerate with `scripts/site-output.mjs`, and let the tests guard it.
- **Do** write desktop hero lengths as comp pixels times `--u`, with px floors on text and touch targets.
- **Do** keep all styling in `site.css` (or a CSP-hashed `<style>` block on a standalone page); the CSP allows no style attributes.
- **Do** give every new raster its provenance: an embedded prompt or a `.json` sidecar next to it.
- **Do** place crystal plates in a section's empty side, screen-blended and masked to fade inside the section.
- **Do** keep 44 px touch targets and a 12 px terminal-type floor on small screens, and a still path for every animation.
- **Do** self-host fonts and media; the site loads nothing from a third-party host.

### Don't:
- **Don't** introduce a saturated color that PrismTTY did not emit; the logo mark, the focus ring and selection are the only exceptions.
- **Don't** add style attributes; the CSP blocks them.
- **Don't** use gradient text, glow, box-shadows, neon or gradient blobs; the logo mark's own gradient stroke stays inside the mark.
- **Don't** add eyebrows or kicker labels above headings.
- **Don't** put a crystal plate under running text, or behind the scope section.
- **Don't** add a second solid button to a page.
- **Don't** imitate output with hand-colored spans; the JS imitation highlighter is retired.
