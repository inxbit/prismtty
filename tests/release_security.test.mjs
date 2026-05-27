import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const read = (path) => readFileSync(path, 'utf8');

test('release workflow gates tag publishing to main and verifies artifact checksums', () => {
  const workflow = read('.github/workflows/release.yml');

  assert.match(workflow, /Verify release tag is on main/);
  assert.match(workflow, /git merge-base --is-ancestor HEAD origin\/main/);
  assert.match(workflow, /shasum -a 256 -c "\$checksum"/);
});

test('privacy scan includes Cargo.lock in sensitive marker checks', () => {
  const script = read('scripts/privacy-scan.sh');

  assert.doesNotMatch(script, /:!Cargo\.lock/);
});
