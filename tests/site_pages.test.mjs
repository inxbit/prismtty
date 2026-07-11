import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  chmodSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import test from 'node:test';

const read = (path) => readFileSync(path, 'utf8');

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
  assert.match(html, /Highlighting, with a clear boundary\./);
  assert.match(html, /feedback wanted/i);
  assert.match(html, /href="https:\/\/github\.com\/inxbit\/prismtty"/);
  assert.match(html, /href="https:\/\/crates\.io\/crates\/prismtty"/);
  assert.match(html, /https:\/\/prismtty\.com\/assets\/prismtty-social-card\.png/);
  // The hero now demonstrates highlighting with JS-driven demos (live terminal,
  // raw/highlighted compare slider, profile tabs) instead of a static SVG image.
  assert.match(html, /data-terminal\b/);
  assert.match(html, /Noise becomes signal\./);
  assert.match(html, /class="proof-rail"/);
  assert.match(html, /data-menu-trigger/);
  assert.match(html, /data-terminal[^>]*>[\s\S]*class="tline"/);
  assert.match(html, /data-terminal[^>]*>[\s\S]*?GigabitEthernet1\/0\/2[\s\S]*?<\/div>/);
  assert.match(html, /data-compare-raw[^>]*>[\s\S]*?GigabitEthernet1\/0\/2[\s\S]*?<\/pre>/);
  assert.match(html, /data-compare-hl[^>]*>[\s\S]*?GigabitEthernet1\/0\/2[\s\S]*?<\/pre>/);
  assert.match(html, /data-profile-body[^>]*>[\s\S]*?GigabitEthernet1\/0\/1[\s\S]*?<\/div>/);
  assert.doesNotMatch(html, /terminal-badge|● live|spectrum-text|hero-facts/);
  assert.match(html, /data-compare\b/);
  assert.match(html, /data-profiles\b/);
  assert.match(html, /id="scope"/);
  assert.equal((html.match(/role="tabpanel"/g) ?? []).length, 1);
  assert.doesNotMatch(html, /feature-kicker|step-number|profile-list/);
  assert.doesNotMatch(html, /show-tech\.txt \| prismtty/);

  const css = read('docs/styles.css');
  assert.match(css, /#45c7d8/);
  assert.match(css, /#8bd450/);
  assert.match(css, /#e678a8/);

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

test('site metadata, crawlers, 404, and social card match the release', () => {
  const index = read('docs/index.html');
  const cargoVersion = read('Cargo.toml').match(/^version\s*=\s*"([^"]+)"/m)[1];
  const imageAlt = 'PrismTTY highlighting network terminal output in a dark terminal interface';

  assert.match(index, /<meta property="og:url" content="https:\/\/prismtty\.com\/">/);
  assert.match(index, /<meta property="og:site_name" content="PrismTTY">/);
  assert.match(index, /<meta property="og:image:type" content="image\/png">/);
  assert.match(index, new RegExp(`<meta property="og:image:alt" content="${imageAlt}">`));
  assert.match(index, /<meta name="twitter:title" content="PrismTTY - Terminal Output Highlighting">/);
  assert.match(index, /<meta name="twitter:description" content="Readable network output, live in your terminal\.">/);
  assert.match(index, new RegExp(`<meta name="twitter:image:alt" content="${imageAlt}">`));

  const structuredDataBlock = index.match(
    /<script type="application\/ld\+json">([\s\S]*?)<\/script>/,
  );
  assert.ok(structuredDataBlock, 'SoftwareApplication JSON-LD exists');
  const structuredData = JSON.parse(structuredDataBlock[1]);
  assert.equal(structuredData['@context'], 'https://schema.org');
  assert.equal(structuredData['@type'], 'SoftwareApplication');
  assert.equal(structuredData.name, 'PrismTTY');
  assert.deepEqual(structuredData.operatingSystem, ['macOS', 'Linux']);
  assert.equal(structuredData.softwareVersion, cargoVersion);
  assert.equal(
    structuredData.downloadUrl,
    `https://github.com/inxbit/prismtty/releases/tag/v${cargoVersion}`,
  );

  const indexCsp = index.match(
    /Content-Security-Policy"\s*content="([^"]+)"/,
  )[1];
  const structuredDataHash = `sha256-${createHash('sha256')
    .update(structuredDataBlock[1], 'utf8')
    .digest('base64')}`;
  assert.ok(indexCsp.includes(`'${structuredDataHash}'`), 'stale JSON-LD script hash');
  assert.doesNotMatch(indexCsp, /unsafe-inline/);

  assert.equal(existsSync('docs/robots.txt'), true);
  assert.equal(
    read('docs/robots.txt'),
    'User-agent: *\nAllow: /\n\nSitemap: https://prismtty.com/sitemap.xml\n',
  );
  assert.equal(existsSync('docs/sitemap.xml'), true);
  const sitemap = read('docs/sitemap.xml');
  assert.equal((sitemap.match(/<loc>https:\/\/prismtty\.com\/<\/loc>/g) ?? []).length, 1);
  assert.match(sitemap, /<lastmod>2026-07-11<\/lastmod>/);
  assert.doesNotMatch(sitemap, /changefreq|priority|www\.prismtty\.com/);

  const notFound = read('docs/404.html');
  assert.match(notFound, /<meta name="theme-color" content="#07090d">/);
  assert.match(notFound, /class="instrument-shell nf-shell"/);
  assert.match(notFound, /class="terminal-lights"/);
  assert.match(notFound, /class="button secondary"/);
  assert.doesNotMatch(notFound, /class="dots"|class="button ghost"|GitHub ↗/);

  const socialSvg = read('docs/assets/prismtty-social-card.svg');
  assert.match(socialSvg, /viewBox="0 0 1200 630"/);
  assert.match(socialSvg, /<title[^>]*>[^<]+<\/title>/);
  assert.match(socialSvg, /<desc[^>]*>[^<]+<\/desc>/);
  assert.match(socialSvg, /\.\/fonts\/space-grotesk-latin\.woff2/);
  assert.match(socialSvg, /\.\/fonts\/jetbrains-mono-latin\.woff2/);
  assert.match(socialSvg, /Noise becomes[\s\S]*signal\./);
  assert.doesNotMatch(socialSvg, /font-family:\s*Inter|fonts\.googleapis|id="beam"|skewX/);

  const socialPng = readFileSync('docs/assets/prismtty-social-card.png');
  assert.deepEqual(
    [...socialPng.subarray(0, 8)],
    [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
  );
  assert.equal(socialPng.readUInt32BE(16), 1200);
  assert.equal(socialPng.readUInt32BE(20), 630);
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
