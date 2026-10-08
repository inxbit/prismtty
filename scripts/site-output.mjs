#!/usr/bin/env node
// Regenerates the real PrismTTY output that prismtty.com shows.
//
// Each fixtures/site/<slug>.<profile>.txt is piped through the release binary
// with that profile forced (-R -p <profile>) in an empty environment, so no
// user or system config can leak in. The ANSI result is kept beside the
// fixture as <slug>.<profile>.ansi, and the pages get two renderings between
// comment markers:
//
//   <!-- raw:<slug> --> ... <!-- /raw:<slug> -->        the input, escaped
//                                                        (optional,
//                                                        docs/index.html only)
//   <!-- output:<slug> --> ... <!-- /output:<slug> -->  the ANSI as spans, in
//                                                        docs/index.html or
//                                                        docs/404.html
//
// The page CSP forbids style attributes, so each color the binary emits on
// either page becomes a class (.c-rrggbb) written between the output-colors
// markers in docs/site.css. tests/site_pages.test.mjs re-renders every .ansi
// snapshot with ansiToHtml() and fails if a page drifts; tests/site_output.rs
// re-runs the binary and fails if a snapshot drifts from real output.
//
// Usage: node scripts/site-output.mjs   (PRISMTTY_BIN overrides the binary,
//        default target/release/prismtty; run `cargo build --release` first)
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const FIXTURE_DIR = 'fixtures/site';
export const PAGE = 'docs/index.html';
export const NOT_FOUND = 'docs/404.html';
export const STYLESHEET = 'docs/site.css';

export function escapeHtml(text) {
  return text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// Fixture names are <slug>.<profile>.txt; the profile is forced on the run.
export function listFixtures(dir = FIXTURE_DIR) {
  return readdirSync(dir)
    .filter((name) => name.endsWith('.txt'))
    .sort()
    .map((name) => {
      const [slug, profile] = name.slice(0, -'.txt'.length).split('.');
      if (!slug || !profile) throw new Error(`${name}: expected <slug>.<profile>.txt`);
      return { slug, profile, txt: join(dir, name), ansi: join(dir, `${slug}.${profile}.ansi`) };
    });
}

// Only the SGR subset the binary emits is accepted: reset, bold, normal
// intensity, default foreground and 24-bit foreground. Anything else throws,
// so the page can never show a sequence it does not understand.
function applySgr(params, state) {
  const codes = params === '' ? [0] : params.split(';').map(Number);
  for (let i = 0; i < codes.length; i += 1) {
    const code = codes[i];
    if (code === 0) {
      state.fg = null;
      state.bold = false;
    } else if (code === 1) {
      state.bold = true;
    } else if (code === 22) {
      state.bold = false;
    } else if (code === 39) {
      state.fg = null;
    } else if (code === 38 && codes[i + 1] === 2) {
      const rgb = codes.slice(i + 2, i + 5);
      if (rgb.length !== 3 || rgb.some((v) => !(v >= 0 && v <= 255))) {
        throw new Error(`bad truecolor sequence: ${params}`);
      }
      state.fg = rgb.map((v) => v.toString(16).padStart(2, '0')).join('');
      i += 4;
    } else {
      throw new Error(`unsupported SGR code ${code} in "${params}"`);
    }
  }
}

// The ANSI as lines of styled runs, { text, fg: 'rrggbb' or null, bold }.
// A trailing newline ends the last line rather than opening an empty one.
// scripts/readme-svgs.mjs draws the README images from the same runs.
export function ansiSegments(ansi) {
  const state = { fg: null, bold: false };
  const lines = [[]];
  const push = (text) => {
    for (const [index, part] of text.split('\n').entries()) {
      if (index > 0) lines.push([]);
      if (part) lines.at(-1).push({ text: part, fg: state.fg, bold: state.bold });
    }
  };
  const pattern = /\x1b\[([0-9;]*)m/g;
  let last = 0;
  for (const match of ansi.matchAll(pattern)) {
    push(ansi.slice(last, match.index));
    applySgr(match[1], state);
    last = match.index + match[0].length;
  }
  push(ansi.slice(last));
  if (lines.some((line) => line.some((segment) => segment.text.includes('\x1b')))) {
    throw new Error('unsupported escape sequence in output');
  }
  if (lines.length > 1 && lines.at(-1).length === 0) lines.pop();
  return lines;
}

export function ansiToHtml(ansi) {
  const html = ansiSegments(ansi)
    .map((line) =>
      line
        .map(({ text, fg, bold }) => {
          const classes = [fg && `c-${fg}`, bold && 'b'].filter(Boolean).join(' ');
          return classes ? `<span class="${classes}">${escapeHtml(text)}</span>` : escapeHtml(text);
        })
        .join(''),
    )
    .join('\n');
  return wrapLines(html);
}

// One element per line, so the page can animate the output line by line.
export function wrapLines(html) {
  return html
    .split('\n')
    .map((line) => `<span class="ln">${line}</span>`)
    .join('\n');
}

export function colorsIn(html) {
  return [...new Set([...html.matchAll(/c-([0-9a-f]{6})/g)].map((m) => m[1]))].sort();
}

export function replaceBetween(source, start, end, body, file) {
  const from = source.indexOf(start);
  const to = source.indexOf(end);
  if (from === -1 || to === -1 || to < from) throw new Error(`${file}: missing ${start} ... ${end}`);
  return source.slice(0, from + start.length) + body + source.slice(to);
}

function runBinary(bin, profile, input) {
  const home = mkdtempSync(join(tmpdir(), 'prismtty-site-'));
  try {
    mkdirSync(join(home, 'xdg'));
    mkdirSync(join(home, 'run'), { mode: 0o700 });
    const result = spawnSync(bin, ['-R', '-p', profile], {
      input,
      encoding: 'utf8',
      env: { HOME: home, XDG_CONFIG_HOME: join(home, 'xdg'), PRISMTTY_RUNTIME_DIR: join(home, 'run') },
    });
    if (result.error) throw result.error;
    if (result.status !== 0) throw new Error(`${bin} exited ${result.status}: ${result.stderr}`);
    return result.stdout;
  } finally {
    rmSync(home, { recursive: true, force: true });
  }
}

function main() {
  const bin = process.env.PRISMTTY_BIN || 'target/release/prismtty';
  const pages = new Map([PAGE, NOT_FOUND].map((file) => [file, readFileSync(file, 'utf8')]));
  const colors = new Set();
  for (const fixture of listFixtures()) {
    const input = readFileSync(fixture.txt, 'utf8');
    const ansi = runBinary(bin, fixture.profile, input);
    writeFileSync(fixture.ansi, ansi);
    const html = ansiToHtml(ansi);
    colorsIn(html).forEach((color) => colors.add(color));
    // The raw rendering is optional: only the hero shows the input itself.
    let page = pages.get(PAGE);
    if (page.includes(`<!-- raw:${fixture.slug} -->`)) {
      page = replaceBetween(page, `<!-- raw:${fixture.slug} -->`, `<!-- /raw:${fixture.slug} -->`, wrapLines(escapeHtml(input.replace(/\n$/, ''))), PAGE);
      pages.set(PAGE, page);
    }
    // The output block lives on whichever page carries its markers.
    const file = [...pages.keys()].find((name) => pages.get(name).includes(`<!-- output:${fixture.slug} -->`)) ?? PAGE;
    pages.set(file, replaceBetween(pages.get(file), `<!-- output:${fixture.slug} -->`, `<!-- /output:${fixture.slug} -->`, html, file));
    console.log(`${fixture.slug}: ${fixture.profile}, ${input.split('\n').length - 1} lines, ${file}`);
  }
  for (const [file, page] of pages) writeFileSync(file, page);

  const rules = [...colors].sort().map((hex) => `.c-${hex} { color: #${hex}; }`).join('\n');
  const css = readFileSync(STYLESHEET, 'utf8');
  writeFileSync(STYLESHEET, replaceBetween(css, '/* output-colors:start */', '/* output-colors:end */', `\n${rules}\n`, STYLESHEET));
  console.log(`colors: ${[...colors].join(' ')}`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
