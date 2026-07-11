import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  chmodSync,
  copyFileSync,
  mkdtempSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  unlinkSync,
  utimesSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import test from 'node:test';

const read = (path) => readFileSync(path, 'utf8');

function stepBody(workflow, stepName) {
  const lines = workflow.split('\n');
  const nameIndex = lines.findIndex(
    (line) => line.trim() === `- name: ${stepName}`,
  );
  assert.notEqual(nameIndex, -1, `${stepName} exists`);
  const stepIndent = lines[nameIndex].match(/^(\s*)-/)[1];
  const nextStepIndex = lines.findIndex(
    (line, index) => index > nameIndex && line.startsWith(`${stepIndent}- `),
  );
  const stepEnd = nextStepIndex === -1 ? lines.length : nextStepIndex;
  const runIndex = lines.findIndex(
    (line, index) =>
      index > nameIndex && index < stepEnd && line.trim() === 'run: |',
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

function jobBody(workflow, jobName) {
  const lines = workflow.split('\n');
  const start = lines.findIndex((line) => line === `  ${jobName}:`);
  assert.notEqual(start, -1, `${jobName} job exists`);
  const end = lines.findIndex(
    (line, index) => index > start && /^  [A-Za-z0-9_-]+:$/.test(line),
  );
  return lines.slice(start, end === -1 ? lines.length : end).join('\n');
}

function runStep(workflow, stepName, env) {
  return spawnSync('bash', ['-c', stepBody(workflow, stepName)], {
    cwd: process.cwd(),
    env: { ...process.env, ...env },
    encoding: 'utf8',
  });
}

function writeFakeCurl(directory) {
  const path = `${directory}/curl`;
  writeFileSync(
    path,
    `#!/usr/bin/env bash
set -euo pipefail
output=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)
      output="$2"
      shift 2
      ;;
    --write-out)
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
if [[ -n "$output" ]]; then
  cp "$FAKE_CURL_RESPONSE" "$output"
  printf '%s' "$FAKE_CURL_STATUS"
else
  cat "$FAKE_CURL_RESPONSE"
fi
`,
  );
  chmodSync(path, 0o755);
}

test('release shell steps remain syntactically valid', () => {
  const workflow = read('.github/workflows/release.yml');
  for (const stepName of [
    'Verify release tag is on main',
    'Validate artifacts',
    'Validate Homebrew formula',
    'Finalize exact release manifest',
    'Package crate for publication verification',
    'Verify existing GitHub release assets',
    'Seal exact release artifact set',
    'Check crates.io publication state',
  ]) {
    const result = spawnSync('bash', ['-n'], {
      input: stepBody(workflow, stepName),
      encoding: 'utf8',
    });
    assert.equal(result.status, 0, `${stepName}: ${result.stderr}`);
  }
});

test('release workflow gates tag publishing to main and verifies artifact checksums', () => {
  const workflow = read('.github/workflows/release.yml');

  assert.match(workflow, /Verify release tag is on main/);
  assert.match(workflow, /git merge-base --is-ancestor HEAD origin\/main/);
  assert.match(workflow, /shasum -a 256 -c "\$checksum"/);
  assert.doesNotMatch(workflow, /--depth(?:=|\s)/);
  assert.equal((workflow.match(/fetch-depth: 0/g) ?? []).length, 3);
});

test('CI and release dependency resolution always honors Cargo.lock', () => {
  const paths = [
    '.github/workflows/ci.yml',
    '.github/workflows/release.yml',
    'scripts/package-release.sh',
  ];
  const dependencyResolvingCommand =
    /^\s*(?:-\s+)?(?:run:\s+)?cargo\s+(?:build|clippy|package|pkgid|publish|run|test)\b/;

  const assertLocked = (path, contents) => {
    for (const [index, line] of contents.split('\n').entries()) {
      if (dependencyResolvingCommand.test(line)) {
        assert.match(line, /\s--locked(?:\s|$)/, `${path}:${index + 1}`);
      }
    }
  };

  for (const path of paths) {
    assertLocked(path, read(path));
  }

  const release = read('.github/workflows/release.yml');
  const mutated = release.replace('run: cargo test --locked', 'run: cargo test');
  assert.notEqual(mutated, release, 'mutation target exists');
  assert.throws(
    () => assertLocked('.github/workflows/release.yml', mutated),
    /\.github\/workflows\/release\.yml:/,
  );
});

test('CI verifies the minimum supported Rust version used for releases', () => {
  const manifest = read('Cargo.toml');
  const ci = read('.github/workflows/ci.yml');
  const release = read('.github/workflows/release.yml');
  const rustVersion = manifest.match(/^rust-version = "([^"]+)"$/m)[1];
  const releaseToolchain = `${rustVersion}.0`;

  assert.match(ci, /name: Minimum supported Rust/);
  assert.match(ci, new RegExp(`toolchain: ${releaseToolchain.replaceAll('.', '\\.')}`));
  assert.match(ci, /cargo test --locked/);
  assert.equal(
    release.split(`toolchain: ${releaseToolchain}`).length - 1,
    3,
    'all release jobs that invoke Cargo use the package MSRV',
  );
});

test('release artifacts use supported runner labels for each architecture', () => {
  const workflow = read('.github/workflows/release.yml');

  assert.match(
    workflow,
    /- os: macos-15\n\s+target_name: darwin-aarch64/,
  );
  assert.match(
    workflow,
    /- os: macos-15-intel\n\s+target_name: darwin-x86_64/,
  );
  assert.doesNotMatch(workflow, /- os: macos-14\n/);
});

test('release publication uses one sealed exact artifact allowlist', () => {
  const workflow = read('.github/workflows/release.yml');
  const sealedPaths = '${{ steps.release-artifacts.outputs.paths }}';

  assert.match(workflow, /release-artifacts\.manifest/);
  assert.match(workflow, /comm -3/);
  assert.match(workflow, /sealed-release-artifacts/);
  assert.equal(workflow.split(sealedPaths).length - 1, 2);
  assert.doesNotMatch(workflow, /sealed-release-artifacts\/\*/);
  assert.doesNotMatch(
    workflow,
    /release-artifacts\/\*\.tar\.gz(?:\.sha256)?/,
  );
});

test('release publication completes recoverable asset work before crate publication', () => {
  const workflow = read('.github/workflows/release.yml');
  const cratePublish = workflow.indexOf('- name: Publish crate');
  const seal = workflow.indexOf('- name: Seal exact release artifact set');
  const attest = workflow.indexOf('- name: Attest release artifacts');
  const release = workflow.indexOf('- name: Publish release');
  const existingRelease = workflow.indexOf(
    '- name: Verify existing GitHub release assets',
  );

  assert.ok(existingRelease < seal, 'existing assets are validated before final sealing');
  assert.ok(seal < attest, 'sealing precedes attestation');
  assert.ok(attest < release, 'attestation precedes release upload');
  assert.ok(release < cratePublish, 'crate publication is the final irreversible operation');
  assert.match(workflow, /Verify existing GitHub release assets/);
});

test('release validation cannot request trusted-publishing credentials', () => {
  const workflow = read('.github/workflows/release.yml');
  const validation = jobBody(workflow, 'release-validation');
  const publication = jobBody(workflow, 'release-publish');

  assert.match(validation, /cargo package --locked/);
  assert.doesNotMatch(validation, /id-token: write/);
  assert.match(publication, /id-token: write/);
  assert.match(publication, /cargo publish --locked --no-verify/);
  assert.doesNotMatch(publication, /cargo (?:build|check|clippy|package|test)\b/);
});

test('release artifact allowlist accepts exact files and rejects additions', () => {
  const workflow = read('.github/workflows/release.yml');
  const directory = mkdtempSync(`${tmpdir()}/prismtty-release-`);
  const artifactDirectory = `${directory}/release-artifacts`;
  const version = read('Cargo.toml').match(/^version = "([^"]+)"/m)[1];

  try {
    mkdirSync(artifactDirectory);
    for (const target of [
      'darwin-aarch64',
      'darwin-x86_64',
      'linux-x86_64',
    ]) {
      const basename = `prismtty-${version}-${target}.tar.gz`;
      const contents = `archive for ${target}\n`;
      const digest = createHash('sha256').update(contents).digest('hex');
      writeFileSync(`${artifactDirectory}/${basename}`, contents);
      writeFileSync(
        `${artifactDirectory}/${basename}.sha256`,
        `${digest}  ${basename}\n`,
      );
    }

    const env = {
      RUNNER_TEMP: directory,
      GITHUB_REF_NAME: `v${version}`,
      GITHUB_OUTPUT: `${directory}/step-output`,
    };
    let result = runStep(workflow, 'Validate artifacts', env);
    assert.equal(result.status, 0, result.stderr || result.stdout);

    const unexpectedArtifact = `${artifactDirectory}/unexpected.tar.gz`;
    writeFileSync(unexpectedArtifact, 'unexpected\n');
    result = runStep(workflow, 'Validate artifacts', env);
    assert.notEqual(result.status, 0);
    assert.match(result.stdout, /do not match the exact allowlist/);
    unlinkSync(unexpectedArtifact);

    const formulaContents = 'class Prismtty\nend\n';
    writeFileSync(`${artifactDirectory}/prismtty.rb`, formulaContents);
    result = runStep(workflow, 'Finalize exact release manifest', env);
    assert.equal(result.status, 0, result.stderr || result.stdout);

    const validatedDirectory = `${directory}/validated-release`;
    const validatedArtifactDirectory = `${validatedDirectory}/release-artifacts`;
    mkdirSync(validatedArtifactDirectory, { recursive: true });
    for (const filename of readdirSync(artifactDirectory)) {
      copyFileSync(
        `${artifactDirectory}/${filename}`,
        `${validatedArtifactDirectory}/${filename}`,
      );
    }
    copyFileSync(
      `${directory}/release-artifacts.manifest`,
      `${validatedDirectory}/release-artifacts.manifest`,
    );
    copyFileSync(
      `${directory}/release-artifacts.sha256`,
      `${validatedDirectory}/release-artifacts.sha256`,
    );

    result = runStep(workflow, 'Seal exact release artifact set', env);
    assert.equal(result.status, 0, result.stderr || result.stdout);
    const sealedPaths = read(env.GITHUB_OUTPUT)
      .match(/paths<<EOF\n([\s\S]+)\nEOF/)[1]
      .split('\n');
    assert.equal(sealedPaths.length, 7);
    assert.ok(sealedPaths.every((path) => path.startsWith(directory)));

    writeFileSync(
      `${validatedArtifactDirectory}/prismtty.rb`,
      `${formulaContents}# changed\n`,
    );
    result = runStep(workflow, 'Seal exact release artifact set', env);
    assert.notEqual(result.status, 0);
    assert.match(result.stdout, /FAILED/);
    writeFileSync(`${validatedArtifactDirectory}/prismtty.rb`, formulaContents);

    writeFileSync(`${validatedArtifactDirectory}/unexpected.tar.gz`, 'unexpected\n');
    result = runStep(workflow, 'Seal exact release artifact set', env);
    assert.notEqual(result.status, 0);
    assert.match(result.stdout, /changed after validation/);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('existing GitHub release assets must be an exact matching subset', () => {
  const workflow = read('.github/workflows/release.yml');
  const directory = mkdtempSync(`${tmpdir()}/prismtty-existing-release-`);
  const artifactDirectory = `${directory}/release-artifacts`;
  const fakeBin = `${directory}/bin`;
  const response = `${directory}/release.json`;

  try {
    mkdirSync(artifactDirectory);
    mkdirSync(fakeBin);
    writeFakeCurl(fakeBin);
    writeFileSync(`${artifactDirectory}/one.txt`, 'one\n');
    writeFileSync(`${artifactDirectory}/two.txt`, 'two\n');
    writeFileSync(`${directory}/release-artifacts.manifest`, 'one.txt\ntwo.txt\n');
    const digest = createHash('sha256').update('one\n').digest('hex');
    const env = {
      RUNNER_TEMP: directory,
      GITHUB_REF_NAME: 'v1.2.3',
      GITHUB_REPOSITORY: 'inxbit/prismtty',
      GH_TOKEN: 'test-token',
      PATH: `${fakeBin}:${process.env.PATH}`,
      FAKE_CURL_RESPONSE: response,
      FAKE_CURL_STATUS: '200',
    };

    writeFileSync(
      response,
      JSON.stringify({
        tag_name: 'v1.2.3',
        assets: [{ name: 'one.txt', digest: `sha256:${digest}` }],
      }),
    );
    let result = runStep(workflow, 'Verify existing GitHub release assets', env);
    assert.equal(result.status, 0, result.stderr || result.stdout);

    writeFileSync(
      response,
      JSON.stringify({
        tag_name: 'v1.2.3',
        assets: [{ name: 'unexpected.txt', digest: `sha256:${digest}` }],
      }),
    );
    result = runStep(workflow, 'Verify existing GitHub release assets', env);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /unexpected release asset/);

    writeFileSync(
      response,
      JSON.stringify({
        tag_name: 'v1.2.3',
        assets: [{ name: 'one.txt', digest: `sha256:${'0'.repeat(64)}` }],
      }),
    );
    result = runStep(workflow, 'Verify existing GitHub release assets', env);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /digest mismatch/);

    env.FAKE_CURL_STATUS = '404';
    result = runStep(workflow, 'Verify existing GitHub release assets', env);
    assert.equal(result.status, 0, result.stderr || result.stdout);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('same-tag release reruns are serialized and exact-version reruns are recoverable', () => {
  const workflow = read('.github/workflows/release.yml');
  const packageJob = jobBody(workflow, 'package');
  const toolchains = [...workflow.matchAll(/toolchain:\s*([^\s]+)/g)].map(
    (match) => match[1],
  );

  assert.match(
    workflow,
    /concurrency:\n\s+group: release-\$\{\{ github\.ref \}\}\n\s+cancel-in-progress: false/,
  );
  assert.doesNotMatch(workflow, /^\s+queue:/m);
  assert.match(workflow, /Check crates\.io publication state/);
  assert.match(workflow, /cargo package --locked/);
  assert.match(workflow, /EXPECTED_CRATE_CHECKSUM/);
  assert.match(workflow, /checksum does not match tagged source/);
  assert.deepEqual(toolchains, ['1.85.0', '1.85.0', '1.85.0']);
  assert.doesNotMatch(workflow, /toolchain:\s*stable/);
  assert.match(
    packageJob,
    /toolchain: 1\.85\.0\n\s+components: rustfmt[\s\S]*\n\s+- name: Format check/,
  );
  assert.match(workflow, /api\/v1\/crates\/prismtty\/\$\{version\}/);
  assert.match(
    workflow,
    /if: needs\.release-validation\.outputs\.crate-published != 'true'/,
  );
  assert.match(workflow, /tag_name: \$\{\{ github\.ref_name \}\}/);
  assert.match(workflow, /overwrite_files: true/);
});

test('crates.io recovery requires exact non-yanked crate checksum', () => {
  const workflow = read('.github/workflows/release.yml');
  const directory = mkdtempSync(`${tmpdir()}/prismtty-crates-state-`);
  const fakeBin = `${directory}/bin`;
  const response = `${directory}/crates-response.json`;
  const output = `${directory}/step-output`;

  try {
    mkdirSync(fakeBin);
    writeFakeCurl(fakeBin);
    const env = {
      RUNNER_TEMP: directory,
      GITHUB_REF_NAME: 'v1.2.3',
      GITHUB_OUTPUT: output,
      PATH: `${fakeBin}:${process.env.PATH}`,
      FAKE_CURL_RESPONSE: response,
      FAKE_CURL_STATUS: '200',
      EXPECTED_CRATE_CHECKSUM: 'a'.repeat(64),
    };

    writeFileSync(
      response,
      JSON.stringify({
        version: { num: '1.2.3', yanked: false, checksum: 'a'.repeat(64) },
      }),
    );
    let result = runStep(workflow, 'Check crates.io publication state', env);
    assert.equal(result.status, 0, result.stderr || result.stdout);
    assert.match(read(output), /published=true/);

    writeFileSync(
      response,
      JSON.stringify({
        version: { num: '1.2.3', yanked: false, checksum: 'b'.repeat(64) },
      }),
    );
    result = runStep(workflow, 'Check crates.io publication state', env);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /checksum does not match tagged source/);

    writeFileSync(response, JSON.stringify({ version: { num: '1.2.3', yanked: false } }));
    result = runStep(workflow, 'Check crates.io publication state', env);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /missing or invalid checksum/);

    writeFileSync(response, JSON.stringify({ version: { num: '1.2.3', yanked: true } }));
    result = runStep(workflow, 'Check crates.io publication state', env);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /yanked/);

    writeFileSync(response, JSON.stringify({ version: { num: '1.2.3' } }));
    result = runStep(workflow, 'Check crates.io publication state', env);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /indeterminate/);

    unlinkSync(output);
    env.FAKE_CURL_STATUS = '404';
    result = runStep(workflow, 'Check crates.io publication state', env);
    assert.equal(result.status, 0, result.stderr || result.stdout);
    assert.match(read(output), /published=false/);

    env.FAKE_CURL_STATUS = '500';
    result = runStep(workflow, 'Check crates.io publication state', env);
    assert.notEqual(result.status, 0);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('release archives are reproducible across source mtime changes', () => {
  const directory = mkdtempSync(`${tmpdir()}/prismtty-package-`);
  const fakeBin = `${directory}/bin`;
  const script = `${directory}/package-release.sh`;
  const sourceFiles = [
    'LICENSE',
    'README.md',
    'completions/prismtty.bash',
    'completions/prismtty.fish',
    'completions/_prismtty',
    'profiles/custom-router.example.yml',
    'target/release/prismtty',
    'target/release/ptty',
    'target/release/ct',
  ];

  try {
    for (const path of ['bin', 'completions', 'profiles', 'target/release']) {
      mkdirSync(`${directory}/${path}`, { recursive: true });
    }
    copyFileSync('scripts/package-release.sh', script);
    chmodSync(script, 0o755);
    for (const path of sourceFiles) {
      writeFileSync(`${directory}/${path}`, `${path}\n`);
      if (path.startsWith('target/release/')) {
        chmodSync(`${directory}/${path}`, 0o755);
      }
    }
    writeFileSync(
      `${fakeBin}/cargo`,
      `#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  pkgid) printf '%s\n' "$FAKE_CARGO_PKGID" ;;
  build) ;;
  *) exit 1 ;;
esac
`,
    );
    chmodSync(`${fakeBin}/cargo`, 0o755);
    const env = {
      ...process.env,
      PATH: `${fakeBin}:${process.env.PATH}`,
      SOURCE_DATE_EPOCH: '1700000000',
      FAKE_CARGO_PKGID: 'path+file:///fixture#1.2.3',
    };
    const runPackage = () =>
      spawnSync('bash', [script, 'darwin-aarch64'], {
        cwd: directory,
        env,
        encoding: 'utf8',
      });
    const archive = `${directory}/dist/prismtty-1.2.3-darwin-aarch64.tar.gz`;

    let result = runPackage();
    assert.equal(result.status, 0, result.stderr || result.stdout);
    const firstDigest = createHash('sha256').update(readFileSync(archive)).digest('hex');
    const listing = spawnSync('tar', ['-tzf', archive], { encoding: 'utf8' });
    assert.equal(listing.status, 0, listing.stderr);
    assert.match(listing.stdout, /prismtty-1\.2\.3-darwin-aarch64\/prismtty\n/);
    assert.match(
      listing.stdout,
      /prismtty-1\.2\.3-darwin-aarch64\/completions\/prismtty\.bash\n/,
    );

    const changedTime = new Date('2030-01-01T00:00:00Z');
    for (const path of sourceFiles) {
      utimesSync(`${directory}/${path}`, changedTime, changedTime);
    }
    result = runPackage();
    assert.equal(result.status, 0, result.stderr || result.stdout);
    const secondDigest = createHash('sha256').update(readFileSync(archive)).digest('hex');
    assert.equal(secondDigest, firstDigest);

    unlinkSync(archive);
    env.FAKE_CARGO_PKGID = 'path+file:///fixture#prismtty@1.2.3';
    result = runPackage();
    assert.equal(result.status, 0, result.stderr || result.stdout);
    assert.equal(
      createHash('sha256').update(readFileSync(archive)).digest('hex'),
      firstDigest,
      'package-name-qualified cargo pkgid must retain the manifest version',
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('privacy scan includes Cargo.lock in sensitive marker checks', () => {
  const script = read('scripts/privacy-scan.sh');

  assert.doesNotMatch(script, /:!Cargo\.lock/);
});

test('release packaging maps macOS arm64 to the published target name', () => {
  const script = read('scripts/package-release.sh');

  assert.match(script, /default_arch="\$\(uname -m\)"/);
  assert.match(script, /"\$\{default_arch\}" == "arm64"/);
  assert.match(script, /default_arch="aarch64"/);
});
