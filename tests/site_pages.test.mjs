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
  assert.match(html, /Live terminal highlighting, not device management\./);
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
  assert.doesNotMatch(html, /terminal-badge|● live|spectrum-text|hero-facts/);
  assert.match(html, /data-compare\b/);
  assert.match(html, /data-profiles\b/);
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
