// Contract for the static site in docs/ that GitHub Pages serves as
// prismtty.com. The Pages, CI and release workflows run it from the
// repository root:
//
//   node --test tests/*.test.mjs
//
// It stays dependency free on purpose: node built-ins plus the generator
// helpers in scripts/site-output.mjs, so the highlighted output on the page
// is checked by the same code that writes it.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  chmodSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, normalize, relative } from 'node:path';
import test from 'node:test';

import {
  ansiToHtml,
  colorsIn,
  escapeHtml,
  listFixtures,
  wrapLines,
} from '../scripts/site-output.mjs';

const read = (path) => readFileSync(path, 'utf8');

const INDEX = 'docs/index.html';
const NOT_FOUND = 'docs/404.html';
const STYLESHEET = 'docs/site.css';
const SCRIPT = 'docs/site.js';

// The owner approved this policy; widening it needs the owner again.
const CSP =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; media-src 'self'; font-src 'self'; base-uri 'none'; form-action 'none'";

const REGENERATE =
  'run `cargo build --release && node scripts/site-output.mjs` to regenerate the page output and the color rules';

// Published crates.io READMEs load these from https://prismtty.com/assets/,
// so they must keep existing even when the site itself stops using them.
const HOTLINKED = [
  'docs/assets/prismtty-logo.svg',
  'docs/assets/prismtty-terminal-demo.svg',
  'docs/assets/prismtty-terminal-preview.svg',
  'docs/assets/prismtty-profile-switching.svg',
  'docs/assets/prismtty-social-card.png',
];
const SOCIAL_CARD = 'docs/assets/prismtty-social-card.png';

// Hosts a plain <a href> may point at. Any other host is a new third party
// and needs the owner's approval before it is added here.
const LINK_HOSTS = new Set(['prismtty.com', 'github.com', 'crates.io', 'docs.rs']);

// One start tag: name, then attributes whose quoted values may hold ">".
const START_TAG =
  /<([a-zA-Z][\w:-]*)((?:\s+[^\s"'>/=]+(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s"'=<>`]+))?)*)\s*\/?>/g;
const ATTRIBUTE = /([^\s"'>/=]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?/g;
const URL_TOKEN = /url\(\s*(['"]?)([^'")]*)\1\s*\)/g;

function attributes(source) {
  const attrs = {};
  for (const m of source.matchAll(ATTRIBUTE)) {
    attrs[m[1].toLowerCase()] = m[2] ?? m[3] ?? m[4] ?? '';
  }
  return attrs;
}

// Every start tag outside comments (the output markers are comments).
function elements(html) {
  return [...html.replace(/<!--[\s\S]*?-->/g, '').matchAll(START_TAG)].map((m) => ({
    name: m[1].toLowerCase(),
    attrs: attributes(m[2]),
  }));
}

function metaContent(html, key) {
  const meta = elements(html).find(
    ({ name, attrs }) => name === 'meta' && (attrs.property === key || attrs.name === key),
  );
  return meta?.attrs.content;
}

function cspOf(file, html) {
  const metas = elements(html).filter(
    ({ name, attrs }) =>
      name === 'meta' && (attrs['http-equiv'] ?? '').toLowerCase() === 'content-security-policy',
  );
  assert.equal(metas.length, 1, `${file}: expected exactly one Content-Security-Policy <meta>`);
  return metas[0].attrs.content;
}

// The text between two markers that must each appear exactly once.
function between(source, start, end, file, hint = '') {
  const from = source.indexOf(start);
  const to = source.indexOf(end);
  const once = (marker, at) => at !== -1 && source.indexOf(marker, at + 1) === -1;
  assert.ok(once(start, from), `${file}: expected exactly one ${start}${hint}`);
  assert.ok(once(end, to), `${file}: expected exactly one ${end}${hint}`);
  assert.ok(from < to, `${file}: ${end} comes before ${start}${hint}`);
  return source.slice(from + start.length, to);
}

// Names the first line where two renderings part, since a custom assert
// message replaces node's own diff.
function firstDifference(actual, expected) {
  const a = actual.split('\n');
  const e = expected.split('\n');
  const found = a.findIndex((line, index) => line !== e[index]);
  const at = found === -1 ? a.length : found;
  return `first difference at line ${at + 1}:\n  page:     ${a[at] ?? '(no line)'}\n  expected: ${e[at] ?? '(no line)'}`;
}

function filesUnder(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    return entry.isDirectory() ? filesUnder(path) : [path];
  });
}

// Root-absolute paths ("/site.css" in 404.html) resolve from docs/, all
// others from the referring file; a directory means its index.html.
function resolveLocal(from, url) {
  const path = decodeURIComponent(url.split(/[?#]/)[0]);
  let target = normalize(path.startsWith('/') ? join('docs', path) : join(dirname(from), path));
  if (path.endsWith('/') || (existsSync(target) && statSync(target).isDirectory())) {
    target = join(target, 'index.html');
  }
  return target;
}

// src, srcset, poster and href of every element on a page.
function htmlReferences(file, html) {
  const refs = [];
  for (const { name, attrs } of elements(html)) {
    const where = (key) => `${file}: <${name} ${key}>`;
    for (const key of ['src', 'href', 'poster']) {
      if (key in attrs) refs.push({ file, name, key, url: attrs[key], rel: attrs.rel ?? '', where: where(key) });
    }
    if ('srcset' in attrs) {
      for (const candidate of attrs.srcset.split(',')) {
        const url = candidate.trim().split(/\s+/)[0];
        refs.push({ file, name, key: 'srcset', url, rel: '', where: where('srcset') });
      }
    }
  }
  return refs;
}

function cssReferences(file, css) {
  const refs = [...css.matchAll(URL_TOKEN)].map((m) => ({
    file, name: 'css', key: 'url()', url: m[2].trim(), rel: '', where: `${file}: url()`,
  }));
  for (const m of css.matchAll(/@import\s+(['"])([^'"]+)\1/g)) {
    refs.push({ file, name: 'css', key: '@import', url: m[2], rel: '', where: `${file}: @import` });
  }
  return refs;
}

const isRemote = (url) => /^(?:[a-z][a-z0-9+.-]*:)?\/\//i.test(url);

function hexToLuminance(hex) {
  const channel = (i) => {
    const v = parseInt(hex.slice(i, i + 2), 16) / 255;
    return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5);
}

function contrast(a, b) {
  const [hi, lo] = [hexToLuminance(a), hexToLuminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

function workflowStepBody(workflow, stepName) {
  const lines = workflow.split('\n');
  const nameIndex = lines.findIndex(
    (line) => line.trim() === `- name: ${stepName}`,
  );
  assert.notEqual(nameIndex, -1, `${stepName} exists`);
  const runIndex = lines.findIndex(
    (line, index) => index > nameIndex && line.trim() === 'run: |',
  );
  assert.notEqual(runIndex, -1, `${stepName} has a shell body`);
  const body = [];
  for (const line of lines.slice(runIndex + 1)) {
    if (line && !line.startsWith('          ')) {
      break;
    }
    body.push(line ? line.slice(10) : '');
  }
  return body.join('\n');
}

test('home page keeps its title, social copy and scope statement', () => {
  const html = read(INDEX);
  assert.match(
    html,
    /<title>PrismTTY - Terminal Output Highlighting<\/title>/,
    `${INDEX}: <title> must stay "PrismTTY - Terminal Output Highlighting"`,
  );
  assert.equal(
    metaContent(html, 'og:description'),
    'Readable network output, live in your terminal.',
    `${INDEX}: og:description must stay "Readable network output, live in your terminal."`,
  );
  assert.match(
    html,
    /<h2\b[^>]*>Live terminal highlighting, not device management\.<\/h2>/,
    `${INDEX}: the scope section needs the h2 "Live terminal highlighting, not device management."`,
  );
  assert.match(html, /feedback wanted/i, `${INDEX}: the page must still ask for feedback ("feedback wanted")`);
  assert.match(html, /href="https:\/\/github\.com\/inxbit\/prismtty"/, `${INDEX}: link the GitHub repository`);
  assert.match(html, /href="https:\/\/crates\.io\/crates\/prismtty"/, `${INDEX}: link the crates.io page`);
  assert.equal(
    metaContent(html, 'og:image'),
    'https://prismtty.com/assets/prismtty-social-card.png',
    `${INDEX}: og:image must be the absolute social card URL`,
  );
});

test('README files keep their public contract', () => {
  const readme = read('README.md');
  assert.match(readme, /https:\/\/prismtty\.com\//);
  assert.match(readme, /\.github\/assets\/prismtty-terminal-demo\.svg/);
  assert.match(readme, /What This Is \/ What This Is Not/);
  assert.match(readme, /Feedback Wanted/);
  assert.doesNotMatch(readme, /show-tech\.txt \| prismtty/);

  const cratesReadme = read('README.crates.md');
  assert.match(cratesReadme, /Installed commands:/);
  assert.match(cratesReadme, /Runtime Reload/);
  assert.doesNotMatch(cratesReadme, /show-tech\.txt \| prismtty/);
});

test('site pages ship the approved CSP', () => {
  const index = read(INDEX);
  assert.equal(
    cspOf(INDEX, index),
    CSP,
    `${INDEX}: the CSP <meta> content must be exactly the owner-approved policy; ask the owner before changing it`,
  );
  // A meta CSP only governs what the parser meets after it.
  const cspAt = index.search(/http-equiv="Content-Security-Policy"/i);
  const firstLoad = index.search(/<(?:link|script|style|img|picture|video|audio|iframe)\b/i);
  assert.ok(cspAt < firstLoad, `${INDEX}: move the CSP <meta> above every <link>, <script> and media element`);

  // 404.html keeps inline blocks, allowed one by one by hash.
  const notFound = read(NOT_FOUND);
  const csp = cspOf(NOT_FOUND, notFound);
  assert.doesNotMatch(csp, /'unsafe-inline'|'unsafe-eval'/, `${NOT_FOUND}: allow inline blocks by sha256 hash, never with 'unsafe-inline' or 'unsafe-eval'`);
  assert.doesNotMatch(csp, /https?:|\*/, `${NOT_FOUND}: the CSP may only name 'self', hashes and data:, never another host`);
  for (const m of notFound.matchAll(/<(style|script)\b([^>]*)>([\s\S]*?)<\/\1>/gi)) {
    if ('src' in attributes(m[2])) continue;
    const hash = `'sha256-${createHash('sha256').update(m[3], 'utf8').digest('base64')}'`;
    assert.ok(
      csp.includes(hash),
      `${NOT_FOUND}: stale ${m[1]} hash; the inline <${m[1]}> block hashes to ${hash}, put that in the CSP after editing the block`,
    );
  }
});

test('every local reference resolves and nothing loads from another host', () => {
  const pages = [INDEX, NOT_FOUND].map((file) => ({ file, html: read(file) }));
  const refs = [];
  for (const { file, html } of pages) {
    refs.push(...htmlReferences(file, html));
    for (const m of html.matchAll(/<style\b[^>]*>([\s\S]*?)<\/style>/gi)) {
      refs.push(...cssReferences(file, m[1]));
    }
  }
  refs.push(...cssReferences(STYLESHEET, read(STYLESHEET)));
  // site.js swaps the hero footage in at run time.
  for (const m of read(SCRIPT).matchAll(/['"`]((?:media|assets)\/[^'"`\s]+)['"`]/g)) {
    refs.push({ file: SCRIPT, name: 'script', key: 'string', url: m[1], rel: '', where: `${SCRIPT}: string` });
  }
  assert.ok(refs.length > 0, 'no references found; the page parser is broken');

  for (const ref of refs) {
    const where = `${ref.where} "${ref.url}"`;
    assert.notEqual(ref.url, '', `${where}: empty reference`);
    if (ref.url.startsWith('#') || /^(?:data|mailto):/i.test(ref.url)) continue;
    if (isRemote(ref.url)) {
      const host = new URL(ref.url, 'https://prismtty.com/').hostname;
      if (ref.name === 'a' && ref.key === 'href') {
        assert.ok(
          LINK_HOSTS.has(host),
          `${where}: links to ${host}, which is not one of ${[...LINK_HOSTS].join(', ')}; new third-party hosts need the owner's approval`,
        );
      } else if (ref.name === 'link' && ref.rel.toLowerCase().split(/\s+/).includes('canonical')) {
        assert.equal(host, 'prismtty.com', `${where}: the canonical URL must be on prismtty.com`);
      } else {
        assert.fail(`${where}: loads from another host; self-host the file under docs/ (the CSP only allows 'self')`);
      }
      continue;
    }
    assert.doesNotMatch(ref.url, /^[a-z][a-z0-9+.-]*:/i, `${where}: unexpected URL scheme`);
    // Pages serves 404.html at any missing path, so only "/..." resolves there.
    if (ref.file === NOT_FOUND) {
      assert.ok(ref.url.startsWith('/'), `${where}: 404.html is served at any depth; use a root-absolute path ("/${ref.url}")`);
    }
    const target = resolveLocal(ref.file, ref.url);
    assert.ok(!relative('docs', target).startsWith('..'), `${where}: points outside docs/, which Pages does not publish`);
    assert.ok(existsSync(target), `${where}: ${target} does not exist; add the file or fix the path`);
  }

  // In-page anchors land on an element, and ids stay unique.
  for (const { file, html } of pages) {
    const ids = elements(html).map(({ attrs }) => attrs.id).filter(Boolean);
    const seen = new Set();
    for (const id of ids) {
      assert.ok(!seen.has(id), `${file}: id="${id}" is used twice`);
      seen.add(id);
    }
    for (const ref of refs.filter((r) => r.file === file && r.url.length > 1 && r.url.startsWith('#'))) {
      assert.ok(seen.has(ref.url.slice(1)), `${ref.where} "${ref.url}": no element has id="${ref.url.slice(1)}"`);
    }
  }
});

test('hotlinked assets stay published at their old paths', () => {
  for (const path of HOTLINKED) {
    assert.ok(
      existsSync(path),
      `${path} is missing; published crates.io READMEs load it from https://prismtty.com/, so it must stay even if the site no longer uses it`,
    );
  }
  // Every prismtty.com URL that the page metadata or a README points at.
  for (const file of [INDEX, 'README.md', 'README.crates.md']) {
    for (const m of read(file).matchAll(/https:\/\/prismtty\.com\/([^\s"'<>)]*)/g)) {
      const target = resolveLocal('docs/index.html', `/${m[1]}`);
      assert.ok(existsSync(target), `${file} points at ${m[0]}, but ${target} does not exist`);
    }
  }

  const png = readFileSync(SOCIAL_CARD);
  assert.ok(
    png.subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])),
    `${SOCIAL_CARD} is not a PNG file`,
  );
  assert.equal(png.toString('latin1', 12, 16), 'IHDR', `${SOCIAL_CARD}: first PNG chunk is not IHDR`);
  assert.deepEqual(
    [png.readUInt32BE(16), png.readUInt32BE(20)],
    [1200, 630],
    `${SOCIAL_CARD} must be 1200x630 for og:image and twitter:image`,
  );
  const index = read(INDEX);
  assert.equal(metaContent(index, 'og:image:width'), '1200', `${INDEX}: og:image:width must match the social card`);
  assert.equal(metaContent(index, 'og:image:height'), '630', `${INDEX}: og:image:height must match the social card`);
});

test('fonts are self-hosted woff2 files with their licenses beside them', () => {
  const css = read(STYLESHEET);
  const faces = [...css.matchAll(/@font-face\s*\{([^}]*)\}/g)].map((m) => m[1]);
  assert.ok(faces.length > 0, `${STYLESHEET}: no @font-face rules found`);
  const fontFiles = new Set();
  for (const face of faces) {
    const urls = [...face.matchAll(URL_TOKEN)].map((m) => m[2].trim());
    assert.ok(urls.length > 0, `${STYLESHEET}: an @font-face rule has no url()`);
    assert.match(face, /format\(\s*["']woff2["']\s*\)/, `${STYLESHEET}: declare format("woff2") in every @font-face`);
    for (const url of urls) {
      const font = resolveLocal(STYLESHEET, url);
      fontFiles.add(font);
      assert.ok(existsSync(font), `${STYLESHEET}: @font-face url "${url}" resolves to ${font}, which does not exist`);
      assert.equal(
        readFileSync(font).toString('latin1', 0, 4),
        'wOF2',
        `${font} is not a woff2 file (missing the wOF2 signature)`,
      );
      // vend-sans-latin-var.woff2 is licensed by OFL-vend-sans.txt.
      const license = join(dirname(font), `OFL-${basename(font, '.woff2').split('-latin')[0]}.txt`);
      assert.ok(existsSync(license), `${font} needs its SIL Open Font License text at ${license}`);
      assert.match(read(license), /SIL Open Font License/i, `${license} does not hold the SIL Open Font License`);
    }
  }
  for (const { name, attrs } of elements(read(INDEX))) {
    if (name === 'link' && attrs.rel === 'preload' && attrs.as === 'font') {
      const font = resolveLocal(INDEX, attrs.href);
      assert.ok(fontFiles.has(font), `${INDEX}: preloads ${attrs.href}, which no @font-face in ${STYLESHEET} uses`);
    }
  }
  for (const file of filesUnder('docs')) {
    const bytes = readFileSync(file);
    for (const host of ['fonts.googleapis', 'fonts.gstatic']) {
      assert.ok(!bytes.includes(host), `${file} mentions ${host}; fonts are self-hosted under docs/assets/fonts`);
    }
  }
});

test('highlighted output on the pages matches the committed snapshots', () => {
  const pages = [INDEX, NOT_FOUND].map((file) => ({ file, html: read(file) }));
  const index = pages[0].html;
  const fixtures = listFixtures();
  assert.ok(fixtures.length > 0, 'fixtures/site has no <slug>.<profile>.txt fixtures');
  const slugs = new Set();
  for (const { slug, txt, ansi } of fixtures) {
    assert.ok(!slugs.has(slug), `fixtures/site: two fixtures share the slug "${slug}"; slugs must be unique`);
    slugs.add(slug);
    assert.ok(existsSync(ansi), `${ansi} is missing; ${REGENERATE}`);
    const hint = `; add both markers where the block goes, then ${REGENERATE}`;
    // Each fixture renders on exactly one page: index.html or 404.html.
    const homes = pages.filter(({ html }) => html.includes(`<!-- output:${slug} -->`));
    assert.equal(
      homes.length,
      1,
      `<!-- output:${slug} --> must appear on exactly one of ${INDEX} and ${NOT_FOUND}${hint}`,
    );
    const { file, html } = homes[0];
    const output = between(html, `<!-- output:${slug} -->`, `<!-- /output:${slug} -->`, file, hint);
    const expected = ansiToHtml(read(ansi));
    assert.ok(
      output === expected,
      `${file}: the output:${slug} block is not ansiToHtml(${ansi}); ${REGENERATE}\n${firstDifference(output, expected)}`,
    );
    if (index.includes(`<!-- raw:${slug} -->`)) {
      const raw = between(index, `<!-- raw:${slug} -->`, `<!-- /raw:${slug} -->`, INDEX, hint);
      const input = wrapLines(escapeHtml(read(txt).replace(/\n$/, '')));
      assert.ok(
        raw === input,
        `${INDEX}: the raw:${slug} block is not the escaped ${txt}; ${REGENERATE}\n${firstDifference(raw, input)}`,
      );
    }
  }
  // Every block on a page comes from a fixture, so none is edited by hand.
  for (const { file, html } of pages) {
    for (const m of html.matchAll(/<!-- (output|raw):(\S+) -->/g)) {
      assert.ok(
        slugs.has(m[2]),
        `${file}: <!-- ${m[1]}:${m[2]} --> has no fixtures/site/${m[2]}.<profile>.txt; add the fixture or remove the block`,
      );
      assert.ok(
        m[1] === 'output' || file === INDEX,
        `${file}: <!-- raw:${m[2]} --> is only generated on ${INDEX}; remove it`,
      );
    }
  }
});

test('every output color class has exactly one generated rule', () => {
  const used = new Set();
  for (const file of [INDEX, NOT_FOUND]) {
    const html = read(file);
    const onPage = [
      ...new Set(
        elements(html)
          .flatMap(({ attrs }) => (attrs.class ?? '').split(/\s+/))
          .filter((name) => /^c-[0-9a-f]{6}$/.test(name))
          .map((name) => name.slice(2)),
      ),
    ].sort();
    const generated = [...html.matchAll(/<!-- output:(\S+) -->([\s\S]*?)<!-- \/output:\1 -->/g)]
      .map((m) => m[2])
      .join('\n');
    assert.deepEqual(
      onPage,
      colorsIn(generated),
      `${file}: c-rrggbb classes belong to the generated output blocks only; style hand-written markup with ${STYLESHEET} classes instead`,
    );
    onPage.forEach((hex) => used.add(hex));
  }
  const block = between(read(STYLESHEET), '/* output-colors:start */', '/* output-colors:end */', STYLESHEET);
  const expected = `\n${[...used].sort().map((hex) => `.c-${hex} { color: #${hex}; }`).join('\n')}\n`;
  assert.ok(
    block === expected,
    `${STYLESHEET}: the output-colors block must hold one ".c-rrggbb { color: #rrggbb; }" rule per color used in ${INDEX} or ${NOT_FOUND}, sorted, and nothing else; ${REGENERATE}\n${firstDifference(block, expected)}`,
  );
});

test('profile tabs are wired to their panels', () => {
  const html = read(INDEX);
  const all = elements(html);
  const byId = new Map(all.filter(({ attrs }) => attrs.id).map((element) => [element.attrs.id, element]));
  const tabs = all.filter(({ attrs }) => attrs.role === 'tab');
  const panels = all.filter(({ attrs }) => attrs.role === 'tabpanel');
  assert.equal(tabs.length, 8, `${INDEX}: expected 8 profile tabs, found ${tabs.length}`);
  assert.equal(
    tabs.filter(({ attrs }) => attrs['aria-selected'] === 'true').length,
    1,
    `${INDEX}: exactly one tab must start with aria-selected="true"`,
  );
  const profiles = new Map(listFixtures().map(({ slug, profile }) => [slug, profile]));
  for (const { attrs } of tabs) {
    assert.ok(attrs.id, `${INDEX}: every role="tab" needs an id for its panel's aria-labelledby`);
    const panel = byId.get(attrs['aria-controls']);
    assert.ok(
      panel?.attrs.role === 'tabpanel',
      `${INDEX}: tab ${attrs.id} has aria-controls="${attrs['aria-controls']}", which is not a role="tabpanel"`,
    );
    assert.equal(
      panel.attrs['aria-labelledby'],
      attrs.id,
      `${INDEX}: panel ${panel.attrs.id} must have aria-labelledby="${attrs.id}" to point back at its tab`,
    );
    // The tab label names the profile its panel's output was generated with.
    const label = html.match(new RegExp(`<button\\b[^>]*\\bid="${attrs.id}"[^>]*>([^<]*)</button>`))?.[1].trim();
    const slug = html.match(new RegExp(`\\bid="${panel.attrs.id}"[^>]*>\\s*<!-- output:(\\S+) -->`))?.[1];
    assert.ok(slug, `${INDEX}: panel ${panel.attrs.id} must open with an <!-- output:<slug> --> block`);
    assert.equal(
      label,
      profiles.get(slug),
      `${INDEX}: tab ${attrs.id} says "${label}" but its panel shows ${slug}, generated with profile "${profiles.get(slug)}"`,
    );
  }
  assert.equal(panels.length, tabs.length, `${INDEX}: every role="tabpanel" needs exactly one tab`);
});

test('hop stack labels mark the profile each hop block was generated with', () => {
  const html = read(INDEX);
  const profiles = new Map(listFixtures().map(({ slug, profile }) => [slug, profile]));
  // Raw markup, not elements(): the output markers are comments.
  const steps = [...html.matchAll(/<li\b[^>]*\bclass="[^"]*\bhop-step\b[^"]*"[^>]*>([\s\S]*?)<\/li>/g)].map((m) => m[1]);
  assert.ok(steps.length > 0, `${INDEX}: no <li class="hop-step"> found`);
  for (const [index, step] of steps.entries()) {
    const where = `${INDEX}: hop step ${index + 1}`;
    const slug = step.match(/<!-- output:(\S+) -->/)?.[1];
    assert.ok(slug, `${where} must hold an <!-- output:<slug> --> block`);
    const stack = step.match(/<p\b[^>]*\bclass="[^"]*\bhop-stack\b[^"]*"[^>]*>([\s\S]*?)<\/p>/)?.[1];
    assert.ok(stack, `${where} needs a <p class="hop-stack"> naming the profile stack`);
    const active = [...stack.matchAll(/<([a-z]+)\b([^>]*)>([^<]*)<\/\1>/g)]
      .filter((m) => (attributes(m[2]).class ?? '').split(/\s+/).includes('on'))
      .map((m) => m[3].trim());
    assert.equal(active.length, 1, `${where}: the hop-stack must mark exactly one profile with class="on"`);
    assert.equal(
      active[0],
      profiles.get(slug),
      `${where}: the hop-stack marks "${active[0]}" active, but ${slug} was generated with profile "${profiles.get(slug)}"`,
    );
  }
});

// Generated images and footage keep their provenance (generator, prompt and
// post-processing) in a <file>.json sidecar; footage may embed the prompt
// as impeccable:prompt metadata instead.
test('generated images and footage carry their provenance', () => {
  const media = [
    ...readdirSync('docs/media').filter((name) => /\.(?:mp4|webp)$/.test(name)).map((name) => join('docs/media', name)),
    ...readdirSync('docs/assets').filter((name) => name.endsWith('.webp')).map((name) => join('docs/assets', name)),
  ];
  assert.ok(media.length > 0, 'no .mp4 or .webp files found under docs/media or docs/assets');
  for (const file of media) {
    const sidecar = `${file}.json`;
    if (!existsSync(sidecar)) {
      assert.ok(
        file.endsWith('.mp4') && readFileSync(file).includes('impeccable:prompt'),
        `${file} needs a ${sidecar} sidecar with a "prompt" saying how it was generated`,
      );
      continue;
    }
    let data;
    assert.doesNotThrow(() => {
      data = JSON.parse(read(sidecar));
    }, `${sidecar} is not valid JSON`);
    assert.ok(
      typeof data.prompt === 'string' && data.prompt.trim() !== '',
      `${sidecar} needs a non-empty "prompt" saying how ${file} was generated`,
    );
  }
});

test('the page shows the version in Cargo.toml', () => {
  const pkg = read('Cargo.toml').split(/^\[/m).find((table) => table.startsWith('package]'));
  const version = pkg?.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  assert.ok(version, 'Cargo.toml: no version in [package]');
  const fix = 'run `node scripts/site-version.mjs` to sync the page with Cargo.toml';
  const html = read(INDEX);
  const blocks = [...html.matchAll(/<script\b[^>]*type="application\/ld\+json"[^>]*>([\s\S]*?)<\/script>/g)];
  assert.equal(blocks.length, 1, `${INDEX}: expected exactly one JSON-LD block`);
  let data;
  assert.doesNotThrow(() => {
    data = JSON.parse(blocks[0][1]);
  }, `${INDEX}: the JSON-LD block is not valid JSON`);
  assert.equal(
    data.softwareVersion,
    version,
    `${INDEX}: JSON-LD softwareVersion must be ${version} from Cargo.toml; ${fix}`,
  );
  assert.equal(
    between(html, '<!-- version -->', '<!-- /version -->', INDEX),
    version,
    `${INDEX}: the text between <!-- version --> and <!-- /version --> must be ${version} from Cargo.toml; ${fix}`,
  );
});

test('site text tokens clear WCAG AA contrast on the ground and chips', () => {
  const root = read(STYLESHEET).match(/:root\s*\{([^}]*)\}/)?.[1];
  assert.ok(root, `${STYLESHEET}: no :root block`);
  const token = (name) => {
    const value = root.match(new RegExp(`--${name}\\s*:\\s*(#[0-9a-fA-F]{6})\\s*;`))?.[1];
    assert.ok(value, `${STYLESHEET}: the first :root block must define --${name} as a #rrggbb color`);
    return value.toLowerCase();
  };
  // Small text (labels, captions, footer) needs 4.5:1 on every surface.
  for (const ink of ['text', 'text-2', 'muted', 'dim']) {
    for (const surface of ['ground', 'chip']) {
      const ratio = contrast(token(ink), token(surface));
      assert.ok(
        ratio >= 4.5,
        `--${ink} ${token(ink)} on --${surface} ${token(surface)} is ${ratio.toFixed(2)}:1; WCAG AA needs 4.5:1`,
      );
    }
  }
});

test('the retired show-tech demo command appears nowhere', () => {
  const needle = 'show-tech.txt | prismtty';
  for (const file of [...filesUnder('docs'), 'README.md', 'README.crates.md']) {
    assert.ok(!readFileSync(file).includes(needle), `${file} still contains "${needle}"`);
  }
});

test('the home page has no inline styles or scripts', () => {
  const html = read(INDEX);
  for (const { name, attrs } of elements(html)) {
    assert.ok(!('style' in attrs), `${INDEX}: <${name} style> is blocked by style-src 'self'; use a class in ${STYLESHEET}`);
    const handler = Object.keys(attrs).find((key) => /^on[a-z]+$/.test(key));
    assert.ok(!handler, `${INDEX}: <${name} ${handler}> is blocked by script-src 'self'; bind it in ${SCRIPT}`);
  }
  assert.doesNotMatch(html, /<style\b/i, `${INDEX}: inline <style> is blocked by style-src 'self'; move it to ${STYLESHEET}`);
  for (const m of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)) {
    const attrs = attributes(m[1]);
    if ('src' in attrs) {
      assert.equal(m[2].trim(), '', `${INDEX}: a <script src> must be empty`);
    } else {
      assert.equal(
        attrs.type,
        'application/ld+json',
        `${INDEX}: inline <script> is blocked by script-src 'self'; move it to ${SCRIPT}`,
      );
    }
  }
});

test('Pages publishing files are in place', () => {
  assert.equal(read('docs/CNAME').trim(), 'prismtty.com', 'docs/CNAME must hold the custom domain prismtty.com');
  assert.equal(existsSync('docs/.nojekyll'), true, 'docs/.nojekyll must exist so Pages publishes docs/ as is');
});

test('Pages production deployment is main-only and globally serialized', () => {
  const workflow = read('.github/workflows/pages.yml');
  const mainOnly =
    "github.event_name != 'pull_request' && github.ref == 'refs/heads/main'";

  assert.match(workflow, /workflow_dispatch:/);
  assert.equal(workflow.split(`if: ${mainOnly}`).length - 1, 2);
  assert.match(
    workflow,
    /deploy:\n[\s\S]*?concurrency:\n\s+group: pages-production\n(?:\s+#.*\n)*\s+cancel-in-progress: false/,
  );
  assert.doesNotMatch(workflow, /^\s+queue:/m);
  assert.doesNotMatch(workflow, /group: pages-\$\{\{ github\.ref \}\}/);
  assert.match(workflow, /- name: Check current Pages content/);
  assert.match(workflow, /if: steps\.main-revision\.outputs\.current == 'true'/);
});

test('Pages freshness gate allows later non-Pages commits and skips newer docs', () => {
  const workflow = read('.github/workflows/pages.yml');
  const directory = mkdtempSync(`${tmpdir()}/prismtty-pages-`);
  const fakeBin = `${directory}/bin`;
  const output = `${directory}/step-output`;

  try {
    mkdirSync(fakeBin);
    writeFileSync(
      `${fakeBin}/git`,
      `#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  fetch) exit 0 ;;
  rev-parse)
    if [[ "$2" == "refs/remotes/origin/main:docs" ]]; then
      printf '%s\n' "$FAKE_CURRENT_DOCS_TREE"
    else
      printf '%s\n' "$FAKE_BUILT_DOCS_TREE"
    fi
    ;;
  *) exit 2 ;;
esac
`,
    );
    chmodSync(`${fakeBin}/git`, 0o755);
    const env = {
      ...process.env,
      PATH: `${fakeBin}:${process.env.PATH}`,
      FAKE_BUILT_DOCS_TREE: 'docs-tree-a',
      FAKE_CURRENT_DOCS_TREE: 'docs-tree-a',
      GITHUB_OUTPUT: output,
      GITHUB_SHA: 'older-pages-commit',
    };
    const body = workflowStepBody(workflow, 'Check current Pages content');

    let result = spawnSync('bash', ['-c', body], { env, encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr || result.stdout);
    assert.match(read(output), /current=true/);

    writeFileSync(output, '');
    env.FAKE_CURRENT_DOCS_TREE = 'docs-tree-b';
    result = spawnSync('bash', ['-c', body], { env, encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr || result.stdout);
    assert.match(read(output), /current=false/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
