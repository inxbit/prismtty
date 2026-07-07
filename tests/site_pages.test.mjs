import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import test from 'node:test';

const read = (path) => readFileSync(path, 'utf8');

test('GitHub Pages site has the expected static contract', () => {
  assert.equal(existsSync('docs/index.html'), true);
  assert.equal(existsSync('docs/styles.css'), true);
  assert.equal(existsSync('docs/script.js'), true);
  assert.equal(existsSync('docs/.nojekyll'), true);
  assert.equal(read('docs/CNAME').trim(), 'prismtty.com');
  assert.equal(existsSync('docs/assets/prismtty-logo.svg'), true);
  assert.equal(existsSync('docs/assets/prismtty-terminal-demo.svg'), true);
  assert.equal(existsSync('docs/assets/prismtty-terminal-preview.svg'), true);
  assert.equal(existsSync('docs/assets/prismtty-profile-switching.svg'), true);
  assert.equal(existsSync('docs/assets/prismtty-social-card.png'), true);

  const html = read('docs/index.html');
  assert.match(html, /<title>PrismTTY - Terminal Output Highlighting<\/title>/);
  assert.match(html, /Readable network output, live in your terminal\./);
  assert.match(html, /Live terminal highlighting, not device management\./);
  assert.match(html, /feedback wanted/i);
  assert.match(html, /href="https:\/\/github\.com\/inxbit\/prismtty"/);
  assert.match(html, /href="https:\/\/crates\.io\/crates\/prismtty"/);
  assert.match(html, /https:\/\/prismtty\.com\/assets\/prismtty-social-card\.png/);
  // The hero now demonstrates highlighting with JS-driven demos (live terminal,
  // raw/highlighted compare slider, profile tabs) instead of a static SVG image.
  assert.match(html, /data-terminal\b/);
  assert.match(html, /data-compare\b/);
  assert.match(html, /data-profiles\b/);
  assert.doesNotMatch(html, /show-tech\.txt \| prismtty/);

  const css = read('docs/styles.css');
  assert.match(css, /#22d3ee/);
  assert.match(css, /#a3e635/);
  assert.match(css, /#f472b6/);

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

test('site pages ship a strict CSP and self-hosted fonts', () => {
  assert.equal(existsSync('docs/assets/fonts/jetbrains-mono-latin.woff2'), true);
  assert.equal(existsSync('docs/assets/fonts/space-grotesk-latin.woff2'), true);
  assert.equal(existsSync('docs/assets/fonts/OFL-jetbrains-mono.txt'), true);
  assert.equal(existsSync('docs/assets/fonts/OFL-space-grotesk.txt'), true);

  const index = read('docs/index.html');
  const notFound = read('docs/404.html');
  for (const page of [index, notFound]) {
    assert.match(page, /http-equiv="Content-Security-Policy"/);
    assert.doesNotMatch(page, /fonts\.googleapis|fonts\.gstatic/);
  }
  assert.match(read('docs/styles.css'), /@font-face/);

  // The 404 CSP allows its inline <style> and <script> by hash; make sure the
  // hashes stay in sync when either block is edited.
  const csp = notFound.match(
    /Content-Security-Policy"\s*content="([^"]+)"/
  )[1];
  const styleBlock = notFound.match(/<style>([\s\S]*?)<\/style>/)[1];
  const scriptBlock = notFound.match(/<script>([\s\S]*?)<\/script>/)[1];
  const hash = (s) =>
    `sha256-${createHash('sha256').update(s, 'utf8').digest('base64')}`;
  assert.ok(csp.includes(`'${hash(styleBlock)}'`), 'stale 404 style hash');
  assert.ok(csp.includes(`'${hash(scriptBlock)}'`), 'stale 404 script hash');
});
