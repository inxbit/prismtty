import assert from 'node:assert/strict';
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
  assert.equal(existsSync('docs/assets/prismtty-terminal-preview.svg'), true);
  assert.equal(existsSync('docs/assets/prismtty-profile-switching.svg'), true);

  const html = read('docs/index.html');
  assert.match(html, /<title>PrismTTY - Terminal Output Highlighting<\/title>/);
  assert.match(html, /Readable network output, live in your terminal\./);
  assert.match(html, /href="https:\/\/github\.com\/inxbit\/prismtty"/);
  assert.match(html, /href="https:\/\/crates\.io\/crates\/prismtty"/);
  assert.match(html, /href="assets\/prismtty-terminal-preview\.svg"/);

  const css = read('docs/styles.css');
  assert.match(css, /#22d3ee/);
  assert.match(css, /#a3e635/);
  assert.match(css, /#f472b6/);

  const readme = read('README.md');
  assert.match(readme, /https:\/\/prismtty\.com\//);
});
