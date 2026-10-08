#!/usr/bin/env node
// Redraws the three README images from the real PrismTTY output that
// scripts/site-output.mjs keeps in fixtures/site/<slug>.<profile>.ansi, in the
// terminal-window look of prismtty.com:
//
//   prismtty-terminal-preview.svg    tab-cisco in one window
//   prismtty-profile-switching.svg   hop1..hop4 beside their profile stacks,
//                                    like the site's hop section
//   prismtty-terminal-demo.svg       the hero block: raw lines appear in gray,
//                                    then each turns into its real colors
//
// Every image goes to docs/assets/ (served by prismtty.com, which the
// crates.io README hotlinks) and to .github/assets/ (README.md), byte for
// byte the same. GitHub shows README images through <img>, so the files hold
// no script, no external reference and no web font: plain <text> in a system
// monospace stack, laid out at 0.6em per character. The demo animates with
// SMIL only, and a prefers-reduced-motion block shows its final frame. Output
// depends on the snapshots alone, so a rerun without snapshot changes writes
// the same bytes.
//
// Usage: node scripts/readme-svgs.mjs   (after node scripts/site-output.mjs)
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { ansiSegments, escapeHtml, listFixtures } from './site-output.mjs';

export const TARGETS = ['docs/assets', '.github/assets'];

const FONT = "SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace";
const ADVANCE = 0.6; // character advance of the monospace stack, in em
const PAD_X = 32;
const PAD_Y = 24;
const BAR = 48;

// The site's tokens (docs/site.css): ground of the window, hairlines, inks.
const INK = {
  window: '#0c0e11',
  hairline: '#23252a',
  row: '#191c21',
  badge: '#2f333b',
  text: '#f2f3f5',
  text2: '#c4c7ce',
  muted: '#8b9099',
  dim: '#7b8087',
  raw: '#979aa2',
  pill: '#23262c',
  pillOn: '#3a414c',
  pillText: '#8a9099',
};

const num = (value) => String(Math.round(value * 100) / 100);
const advance = (chars, size) => chars * size * ADVANCE;
const columns = (line) => line.reduce((sum, { text }) => sum + text.length, 0);

function snapshot(slug) {
  const fixture = listFixtures().find((f) => f.slug === slug);
  if (!fixture) throw new Error(`fixtures/site has no ${slug}.<profile>.txt`);
  return { profile: fixture.profile, lines: ansiSegments(readFileSync(fixture.ansi, 'utf8')) };
}

function text(attrs, body) {
  return `<text${attrs} xml:space="preserve">${body}</text>`;
}

function runs(line) {
  return line
    .map(({ text: run, fg, bold }) => {
      const attrs = `${fg ? ` fill="#${fg}"` : ''}${bold ? ' font-weight="700"' : ''}`;
      return attrs ? `<tspan${attrs}>${escapeHtml(run)}</tspan>` : escapeHtml(run);
    })
    .join('');
}

// One terminal line per <text>; top is the top of the first line box.
function block(lines, x, top, size, extra = () => '') {
  const height = Math.round(size * 1.625);
  const baseline = (height - size) / 2 + size * 0.78;
  return lines.map((line, i) => text(` x="${num(x)}" y="${num(top + i * height + baseline)}"${extra(i)}`, runs(line)));
}

// The window frame and its title bar, as the site draws .term and .term-bar.
function frame(width, height, badge) {
  const badgeWidth = advance(badge.length, 13) + 20;
  const badgeX = width - 24 - badgeWidth;
  return [
    `<rect x="0.5" y="0.5" width="${num(width - 1)}" height="${num(height - 1)}" rx="14" fill="${INK.window}" stroke="${INK.hairline}"/>`,
    `<path d="M1 ${BAR + 0.5}H${num(width - 1)}" stroke="${INK.hairline}"/>`,
    text(` x="24" y="29" font-size="14" fill="${INK.muted}"`, 'ptty /bin/zsh'),
    `<rect x="${num(badgeX)}" y="11.5" width="${num(badgeWidth)}" height="25" rx="12.5" fill="none" stroke="${INK.badge}"/>`,
    text(` x="${num(badgeX + badgeWidth / 2)}" y="28.5" font-size="13" fill="${INK.text2}" text-anchor="middle"`, escapeHtml(badge)),
  ];
}

// Wide enough for the longest line and for the title bar.
function windowWidth(content, badge) {
  const bar = 24 + advance('ptty /bin/zsh'.length, 14) + 40 + advance(badge.length, 13) + 20 + 24;
  return Math.ceil(Math.max(content, bar));
}

function svg({ width, height, title, desc, style = [], body }) {
  return [
    `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img" aria-labelledby="title desc">`,
    `  <title id="title">${escapeHtml(title)}</title>`,
    `  <desc id="desc">${escapeHtml(desc)}</desc>`,
    '  <style>',
    '    text { white-space: pre; }',
    ...style.map((line) => `    ${line}`),
    '  </style>',
    `  <g font-family="${FONT}" fill="${INK.text2}">`,
    ...body.map((line) => `    ${line}`),
    '  </g>',
    '</svg>',
    '',
  ].join('\n');
}

export function preview() {
  const size = 16;
  const badge = 'generic + cisco';
  const { lines } = snapshot('tab-cisco');
  const width = windowWidth(PAD_X * 2 + advance(Math.max(...lines.map(columns)), size), badge);
  const height = BAR + PAD_Y * 2 + lines.length * Math.round(size * 1.625);
  return svg({
    width,
    height,
    title: 'PrismTTY highlighting Cisco output',
    desc: 'Real PrismTTY output with the cisco profile over a synthetic session: show version, show interfaces description and show ip route, with prompts, interfaces, link states and addresses in color.',
    body: [...frame(width, height, badge), `<g font-size="${size}">`, ...block(lines, PAD_X, BAR + PAD_Y, size).map((l) => `  ${l}`), '</g>'],
  });
}

// The stack each hop shows, as in the hop section of docs/index.html: "on" is
// the active profile (and must be the one its block was generated with), "op"
// the push or pop, "struck" the profile just popped.
const HOPS = [
  { slug: 'hop1', stack: [['on', 'linux-unix']] },
  { slug: 'hop2', stack: [['', 'linux-unix'], ['op', '+'], ['on', 'cisco']] },
  { slug: 'hop3', stack: [['', 'linux-unix'], ['', 'cisco'], ['op', '+'], ['on', 'juniper']] },
  { slug: 'hop4', stack: [['', 'linux-unix'], ['on', 'cisco'], ['op', '−'], ['struck', 'juniper']] },
];
const LABEL = 14;
const PILL_PAD = 8;
const LABEL_GAP = 8;

const labelWidth = ([kind, name]) => advance(name.length, LABEL) + (kind === 'on' || kind === '' ? PILL_PAD * 2 : 0);
const stackWidth = (stack) => stack.reduce((sum, label) => sum + labelWidth(label), 0) + LABEL_GAP * (stack.length - 1);

function stackLabels(stack, top) {
  const center = top + 12;
  const baseline = center + 4.5;
  const out = [];
  let x = PAD_X;
  for (const label of stack) {
    const [kind, name] = label;
    const width = labelWidth(label);
    const middle = num(x + width / 2);
    if (kind === 'op') {
      out.push(text(` x="${middle}" y="${num(baseline)}" fill="${INK.muted}" text-anchor="middle"`, name));
    } else if (kind === 'struck') {
      out.push(text(` x="${middle}" y="${num(baseline)}" fill="${INK.dim}" text-anchor="middle"`, name));
      out.push(`<path d="M${num(x)} ${num(baseline - LABEL * 0.3)}H${num(x + width)}" stroke="${INK.dim}"/>`);
    } else {
      const on = kind === 'on';
      out.push(`<rect x="${num(x + 0.5)}" y="${num(center - 12 + 0.5)}" width="${num(width - 1)}" height="23" rx="7.5" fill="none" stroke="${on ? INK.pillOn : INK.pill}"/>`);
      out.push(text(` x="${middle}" y="${num(baseline)}" fill="${on ? INK.text : INK.pillText}" text-anchor="middle"`, name));
    }
    x += width + LABEL_GAP;
  }
  return out;
}

export function profileSwitching() {
  const size = 16;
  const line = Math.round(size * 1.625);
  const badge = 'generic + top of stack';
  const caption = 'Real PrismTTY output for the profile marked on the left.';
  const hops = HOPS.map(({ slug, stack }) => {
    const { profile, lines } = snapshot(slug);
    const active = stack.find(([kind]) => kind === 'on')[1];
    if (active !== profile) throw new Error(`${slug}: the stack marks ${active}, but the block was generated with ${profile}`);
    return { stack, lines };
  });
  const outX = PAD_X + Math.ceil(Math.max(...hops.map(({ stack }) => stackWidth(stack)))) + 36;
  const content = outX + advance(Math.max(...hops.flatMap(({ lines }) => lines.map(columns))), size) + PAD_X;
  const width = windowWidth(Math.max(content, PAD_X * 2 + advance(caption.length, 14)), badge);

  const body = [];
  let top = BAR;
  for (const [index, { stack, lines }] of hops.entries()) {
    if (index > 0) body.push(`<path d="M1 ${top + 0.5}H${width - 1}" stroke="${INK.row}"/>`);
    body.push(`<g font-size="${LABEL}">`, ...stackLabels(stack, top + PAD_Y + (line - 24) / 2).map((l) => `  ${l}`), '</g>');
    body.push(`<g font-size="${size}">`, ...block(lines, outX, top + PAD_Y, size).map((l) => `  ${l}`), '</g>');
    top += PAD_Y * 2 + lines.length * line;
  }
  body.push(`<path d="M1 ${top + 0.5}H${width - 1}" stroke="${INK.hairline}"/>`);
  body.push(text(` x="${PAD_X}" y="${top + 37}" font-size="14" fill="${INK.muted}"`, escapeHtml(caption)));
  const height = top + 60;

  return svg({
    width,
    height,
    title: 'PrismTTY switching profiles across ssh hops',
    desc: 'One wrapped shell across four hops of a synthetic session, each block real PrismTTY output for the profile marked on its left: linux-unix on the workstation, cisco pushed after ssh to edge-sw1, juniper pushed after ssh to core-rtr, and juniper popped when that connection closes.',
    body: [...frame(width, height, badge), ...body],
  });
}

// Demo timeline, in seconds of one loop.
const LOOP = 10;
const RAW_START = 0.25;
const RAW_STEP = 0.3;
const RAW_FADE = 0.25;
const SWEEP_START = 2.6;
const SWEEP_STEP = 0.35;
const SWEEP = 0.9;
const FADE_OUT = 9.2;

const keyTimes = (...seconds) => seconds.map((s) => String(Math.round((s / LOOP) * 10000) / 10000)).join(';');
const animate = (attribute, values, ...seconds) =>
  `<animate attributeName="${attribute}" dur="${LOOP}s" repeatCount="indefinite" values="${values.join(';')}" keyTimes="${keyTimes(...seconds)}"/>`;

export function demo() {
  const size = 18;
  const badge = 'generic + cisco';
  const { lines } = snapshot('hero');
  const width = windowWidth(PAD_X * 2 + advance(Math.max(...lines.map(columns)), size), badge);
  const height = BAR + PAD_Y * 2 + lines.length * Math.round(size * 1.625);
  // One band per line: the colored copy's band slides in from the left while
  // the gray copy's band slides out to the right in step. All bands of a copy
  // share one clipPath; with a clipPath per line, WebKit drew the lines still
  // waiting for their sweep with the attribute values instead of the animated
  // ones. The attribute values are the final frame, so a renderer without
  // SMIL shows the colored output.
  const top = BAR + PAD_Y;
  const line = Math.round(size * 1.625);
  const from = PAD_X - 4;
  const span = width - from;
  const bands = (id, x, values) => [
    `<clipPath id="${id}">`,
    ...lines.map((_, i) => {
      const start = SWEEP_START + i * SWEEP_STEP;
      return `  <rect x="${x}" y="${top + i * line}" width="${span}" height="${line}">${animate('x', values, 0, start, start + SWEEP, LOOP)}</rect>`;
    }),
    '</clipPath>',
  ];
  const plain = lines.map((segments) => segments.map(({ text: run }) => ({ text: run, fg: null, bold: false })));
  const rawLines = block(plain, PAD_X, top, size, () => ' opacity="0"').map((l, i) => {
    const start = RAW_START + i * RAW_STEP;
    return l.replace('</text>', `${animate('opacity', [0, 0, 1, 1], 0, start, start + RAW_FADE, LOOP)}</text>`);
  });
  const hlLines = block(lines, PAD_X, top, size);
  return svg({
    width,
    height,
    title: 'Animated PrismTTY terminal demo',
    desc: 'Raw Cisco output from a synthetic session appears line by line in gray, then each line turns into the colors the cisco profile gives it: prompts, interfaces, up and down states and addresses. Real PrismTTY output.',
    style: [
      '@media (prefers-reduced-motion: reduce) {',
      '  .raw { display: none; }',
      '  .hl { clip-path: none !important; }',
      '  .play { opacity: 1 !important; }',
      '}',
    ],
    body: [
      ...frame(width, height, badge),
      '<defs>',
      ...bands('hl', from, [from - span, from - span, from, from]).map((l) => `  ${l}`),
      ...bands('raw', width, [from, from, width, width]).map((l) => `  ${l}`),
      '</defs>',
      `<g class="play" font-size="${size}">`,
      `  ${animate('opacity', [1, 1, 0, 0], 0, FADE_OUT, FADE_OUT + 0.4, LOOP)}`,
      `  <g class="raw" fill="${INK.raw}" clip-path="url(#raw)">`,
      ...rawLines.map((l) => `    ${l}`),
      '  </g>',
      '  <g class="hl" clip-path="url(#hl)">',
      ...hlLines.map((l) => `    ${l}`),
      '  </g>',
      '</g>',
    ],
  });
}

export const IMAGES = {
  'prismtty-terminal-preview.svg': preview,
  'prismtty-profile-switching.svg': profileSwitching,
  'prismtty-terminal-demo.svg': demo,
};

function main() {
  for (const [name, draw] of Object.entries(IMAGES)) {
    const image = draw();
    for (const dir of TARGETS) writeFileSync(join(dir, name), image);
    const [, width, height] = image.match(/width="(\d+)" height="(\d+)"/);
    console.log(`${name}: ${width}x${height}, ${TARGETS.join(' and ')}`);
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
