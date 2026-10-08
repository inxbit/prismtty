#!/usr/bin/env node
// Keeps the version prismtty.com shows in step with Cargo.toml.
//
// The [package] version is written into docs/index.html in two places:
//
//   "softwareVersion": "<version>"                 the JSON-LD block
//   <!-- version --><version><!-- /version -->     the footer
//
// The page is only rewritten when one of them differs. With --check nothing
// is written: a mismatch prints what differs and exits 1, so a release that
// bumped Cargo.toml without this step fails the site test.
//
// Usage: node scripts/site-version.mjs           (run from the repo root)
//        node scripts/site-version.mjs --check
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { PAGE, replaceBetween } from './site-output.mjs';

export const MANIFEST = 'Cargo.toml';

const LD_VERSION = /("softwareVersion":\s*")([^"]*)(")/;
const FOOTER_VERSION = /<!-- version -->([^<]*)<!-- \/version -->/;

// Only the [package] table counts; dependency tables carry versions too.
export function packageVersion(toml) {
  let table = '';
  for (const line of toml.split('\n')) {
    if (/^\s*\[/.test(line)) {
      table = line.trim();
    } else if (table === '[package]') {
      const match = line.match(/^\s*version\s*=\s*"([^"]+)"/);
      if (match) return match[1];
    }
  }
  throw new Error(`${MANIFEST}: no version in [package]`);
}

export function pageVersions(html) {
  const ld = html.match(LD_VERSION);
  const footer = html.match(FOOTER_VERSION);
  if (!ld) throw new Error(`${PAGE}: missing JSON-LD "softwareVersion"`);
  if (!footer) throw new Error(`${PAGE}: missing <!-- version --> ... <!-- /version -->`);
  return { 'JSON-LD softwareVersion': ld[2], footer: footer[1] };
}

export function stampVersion(html, version) {
  const stamped = html.replace(LD_VERSION, (_, open, _old, close) => open + version + close);
  return replaceBetween(stamped, '<!-- version -->', '<!-- /version -->', version, PAGE);
}

function main() {
  const args = process.argv.slice(2);
  const check = args.includes('--check');
  const unknown = args.filter((arg) => arg !== '--check');
  if (unknown.length) throw new Error(`unknown argument: ${unknown.join(' ')}`);

  const version = packageVersion(readFileSync(MANIFEST, 'utf8'));
  const page = readFileSync(PAGE, 'utf8');
  const stale = Object.entries(pageVersions(page)).filter(([, shown]) => shown !== version);
  if (stale.length === 0) {
    console.log(`${PAGE}: ${version}, matches ${MANIFEST}`);
    return;
  }
  const found = stale.map(([where, shown]) => `${where} is "${shown}"`).join(', ');
  if (check) {
    console.error(`${PAGE} is out of date: ${MANIFEST} has ${version} but ${found}.`);
    console.error('Run: node scripts/site-version.mjs');
    process.exit(1);
  }
  writeFileSync(PAGE, stampVersion(page, version));
  console.log(`${PAGE}: ${found}, now ${version}`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
