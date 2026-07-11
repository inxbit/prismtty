import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import {
  copyFileSync,
  mkdtempSync,
  mkdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, resolve } from 'node:path';
import test from 'node:test';
import { gzipSync } from 'node:zlib';

const privacyScan = resolve('scripts/privacy-scan.sh');
const sensitiveFixturePath = [
  '',
  'Users',
  'example',
  'Documents',
  'customer.pcap',
].join('/');

function runPrivacyScan(fixture, fixturePath = 'fixture.txt', tracked = true) {
  const directory = mkdtempSync(`${tmpdir()}/prismtty-privacy-`);
  try {
    mkdirSync(`${directory}/scripts`);
    copyFileSync(privacyScan, `${directory}/scripts/privacy-scan.sh`);
    const fixtureFile = `${directory}/${fixturePath}`;
    mkdirSync(dirname(fixtureFile), { recursive: true });
    writeFileSync(fixtureFile, fixture);
    execFileSync('git', ['init', '--quiet'], { cwd: directory });
    execFileSync('git', ['add', 'scripts/privacy-scan.sh'], { cwd: directory });
    if (tracked) {
      execFileSync('git', ['add', fixturePath], { cwd: directory });
    }
    return spawnSync('bash', ['scripts/privacy-scan.sh'], {
      cwd: directory,
      encoding: 'utf8',
    });
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

test('privacy scan permits only synthetic IPv6 documentation ranges', () => {
  const result = runPrivacyScan(
    'IPv4 0.0.0.0 wildcard 0.0.0.255 unspecified :: loopback ::1 docs [2001:db8::1] prefix 2001:db8:abcd::/48\n',
  );

  assert.equal(result.status, 0, result.stderr || result.stdout);
});

for (const [label, address] of [
  ['IPv6 unique-local address', ['fd12', '3456', '789a', '', '1'].join(':')],
  ['IPv6 scoped link-local address', `[${['fe80', '', 'abcd'].join(':')}%en0]`],
  ['IPv6 public address', ['2606', '4700', '4700', '', '1111'].join(':')],
  ['IPv6 public prefix', `${['2606', '4700', '', ''].join(':')}/32`],
]) {
  test(`privacy scan rejects ${label}`, () => {
    const result = runPrivacyScan(`${address}\n`);

    assert.notEqual(result.status, 0);
    assert.match(result.stdout, /IPv6 address outside documentation ranges found/);
    assert.match(result.stdout, /fixture\.txt/);
  });
}

test('privacy scan rejects non-fixture IPv4 zero-block addresses', () => {
  const address = ['0', '1', '2', '3'].join('.');
  const result = runPrivacyScan(`${address}\n`);

  assert.notEqual(result.status, 0);
  assert.match(result.stdout, /IPv4 address outside documentation ranges found/);
});

test('privacy scan checks CI workflow text for sensitive paths and hosts', () => {
  const result = runPrivacyScan(
    `env:\n  CAPTURE: ${sensitiveFixturePath}\n`,
    '.github/workflows/ci.yml',
  );

  assert.notEqual(result.status, 0);
  assert.match(result.stdout, /\.github\/workflows\/ci\.yml/);
  assert.match(result.stdout, /Sensitive real-world capture marker found/);
});

test('privacy scan checks untracked files before they are staged', () => {
  const result = runPrivacyScan(
    `${sensitiveFixturePath}\n`,
    'new-fixture.txt',
    false,
  );

  assert.notEqual(result.status, 0);
  assert.match(result.stdout, /new-fixture\.txt/);
  assert.match(result.stdout, /Sensitive real-world capture marker found/);
});

test('privacy scan rejects binary capture artifacts', () => {
  const privateAddress = ['10', '0', '0', '1'].join('.');
  const result = runPrivacyScan(
    Buffer.concat([Buffer.from([0]), Buffer.from(`${privateAddress}\n`)]),
    'capture.pcap',
  );

  assert.notEqual(result.status, 0);
  assert.match(result.stdout, /Capture artifacts are not allowed/);
  assert.match(result.stdout, /capture\.pcap/);
});

test('privacy scan rejects compressed capture artifacts without banning archives', () => {
  const capture = Buffer.concat([
    Buffer.from([0xd4, 0xc3, 0xb2, 0xa1]),
    Buffer.from('synthetic capture payload\n'),
  ]);
  const rejected = runPrivacyScan(gzipSync(capture), 'capture.PCAP.GZ');

  assert.notEqual(rejected.status, 0);
  assert.match(rejected.stdout, /Capture artifacts are not allowed/);
  assert.match(rejected.stdout, /capture\.PCAP\.GZ/);

  const untracked = runPrivacyScan(gzipSync(capture), 'capture.pcap.xz', false);
  assert.notEqual(untracked.status, 0);
  assert.match(untracked.stdout, /capture\.pcap\.xz/);

  const allowed = runPrivacyScan(
    gzipSync(Buffer.from('synthetic text fixture\n')),
    'synthetic-fixtures.gz',
  );
  assert.equal(allowed.status, 0, allowed.stderr || allowed.stdout);
});
