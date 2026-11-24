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

test('formula generation rejects a version Homebrew cannot derive from the URL', () => {
  // The formula carries no explicit `version`, so Homebrew reads it from the
  // download URL. A hyphenated pre-release makes it read the `x86_64` in the
  // file name instead and report 86.64, which outranks every later release.
  const directory = mkdtempSync(`${tmpdir()}/prismtty-formula-`);
  try {
    const generate = (version) => {
      for (const target of ['darwin-aarch64', 'darwin-x86_64', 'linux-x86_64']) {
        writeFileSync(
          `${directory}/prismtty-${version}-${target}.tar.gz.sha256`,
          `${'a'.repeat(64)}  prismtty-${version}-${target}.tar.gz\n`,
        );
      }
      return spawnSync('bash', ['scripts/generate-homebrew-formula.sh', `${directory}/prismtty.rb`, directory], {
        encoding: 'utf8',
        env: { ...process.env, PRISMTTY_VERSION: version },
      });
    };

    for (const rejected of ['2.0.0-rc1', '2.0.0-beta.1', '1.2', '1.2.3rc4']) {
      const result = generate(rejected);
      assert.notEqual(result.status, 0, `generator accepted version ${rejected}`);
      assert.match(result.stderr, /refusing to generate a formula/);
    }

    const accepted = generate('1.2.3');
    assert.equal(accepted.status, 0, accepted.stderr);
    const formula = read(`${directory}/prismtty.rb`);
    assert.match(formula, /url "[^"]*\/v1\.2\.3\/prismtty-1\.2\.3-darwin-aarch64\.tar\.gz"/);
    assert.doesNotMatch(formula, /^\s*version "/m);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('formula installs the fish completions under every command name', () => {
  // fish only autoloads a completion file named after the command being
  // completed, so `ptty <TAB>` and `ct <TAB>` need ptty.fish and ct.fish even
  // though prismtty.fish already holds their completions.
  const directory = mkdtempSync(`${tmpdir()}/prismtty-formula-`);
  try {
    for (const target of ['darwin-aarch64', 'darwin-x86_64', 'linux-x86_64']) {
      writeFileSync(
        `${directory}/prismtty-1.2.3-${target}.tar.gz.sha256`,
        `${'a'.repeat(64)}  prismtty-1.2.3-${target}.tar.gz\n`,
      );
    }
    const result = spawnSync('bash', ['scripts/generate-homebrew-formula.sh', `${directory}/prismtty.rb`, directory], {
      encoding: 'utf8',
      env: { ...process.env, PRISMTTY_VERSION: '1.2.3' },
    });
    assert.equal(result.status, 0, result.stderr);

    const formula = read(`${directory}/prismtty.rb`);
    assert.match(formula, /^\s*fish_completion\.install "completions\/prismtty\.fish"$/m);
    for (const command of ['ptty', 'ct']) {
      assert.match(
        formula,
        new RegExp(`^\\s*fish_completion\\.install_symlink fish_completion/"prismtty\\.fish" => "${command}\\.fish"$`, 'm'),
        `formula does not install ${command}.fish`,
      );
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('every workflow action is pinned to a full commit SHA', () => {
  // A mutable tag (`@v4`) can be repointed at attacker code that then runs
  // with the job's token; a 40-hex SHA cannot. Local `./` actions are exempt.
  const dir = '.github/workflows';
  const unpinned = [];
  for (const file of readdirSync(dir).filter((name) => /\.ya?ml$/.test(name))) {
    const lines = read(`${dir}/${file}`).split('\n');
    lines.forEach((line, index) => {
      const match = line.match(/^\s*-?\s*uses:\s*(\S+)/);
      if (!match || match[1].startsWith('./')) return;
      if (!/^[^@\s]+@[0-9a-f]{40}$/.test(match[1])) {
        unpinned.push(`${file}:${index + 1}: ${match[1]}`);
      }
    });
  }
  assert.deepEqual(unpinned, [], 'workflow actions must be pinned to a commit SHA');
});

test('cargo audit stays pinned to an exact prebuilt version', () => {
  const ci = read('.github/workflows/ci.yml');
  const release = read('.github/workflows/release.yml');
  // Prebuilt binary via a SHA-pinned installer: no compile on the release
  // toolchain, and an upstream cargo-audit release cannot change what runs.
  const installer = /uses: taiki-e\/install-action@[0-9a-f]{40} # v\d+\.\d+\.\d+\n\s+with:\n\s+tool: cargo-audit@0\.22\.1\n/g;

  assert.equal((ci.match(installer) ?? []).length, 1);
  assert.equal((release.match(installer) ?? []).length, 1);
  assert.doesNotMatch(ci, /cargo install cargo-audit/);
  assert.doesNotMatch(release, /cargo install cargo-audit/);
  assert.match(ci, /cargo audit --deny warnings/);
  assert.match(release, /cargo audit --deny warnings/);
  assert.doesNotMatch(ci, /rustsec\/audit-check/);
  assert.doesNotMatch(release, /rustsec\/audit-check/);
  assert.doesNotMatch(jobBody(ci, 'supply-chain'), /checks: write/);
  assert.doesNotMatch(jobBody(release, 'release-validation'), /checks: write/);
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
    // The fixture's binaries are under ./target; a CARGO_TARGET_DIR inherited
    // from the developer's shell would send the script elsewhere.
    const { CARGO_TARGET_DIR: _inherited, ...inheritedEnv } = process.env;
    const env = {
      ...inheritedEnv,
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

test('formula installs the bash completions under every command name', () => {
  // bash-completion loads a completion file on demand by the name of the
  // command being completed, so `ptty <TAB>` and `ct <TAB>` need files named
  // ptty and ct even though the prismtty file holds their completions.
  const directory = mkdtempSync(`${tmpdir()}/prismtty-formula-`);
  try {
    for (const target of ['darwin-aarch64', 'darwin-x86_64', 'linux-x86_64']) {
      writeFileSync(
        `${directory}/prismtty-1.2.3-${target}.tar.gz.sha256`,
        `${'a'.repeat(64)}  prismtty-1.2.3-${target}.tar.gz\n`,
      );
    }
    const result = spawnSync('bash', ['scripts/generate-homebrew-formula.sh', `${directory}/prismtty.rb`, directory], {
      encoding: 'utf8',
      env: { ...process.env, PRISMTTY_VERSION: '1.2.3' },
    });
    assert.equal(result.status, 0, result.stderr);

    const formula = read(`${directory}/prismtty.rb`);
    assert.match(formula, /^\s*bash_completion\.install "completions\/prismtty\.bash" => "prismtty"$/m);
    for (const command of ['ptty', 'ct']) {
      assert.match(
        formula,
        new RegExp(`^\\s*bash_completion\\.install_symlink bash_completion/"prismtty" => "${command}"$`, 'm'),
        `formula does not install a bash completion named ${command}`,
      );
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('release packaging takes the binaries from the cargo target directory in use', () => {
  // With CARGO_TARGET_DIR set, cargo builds there. The script copied from
  // ./target/release regardless, so it packaged whatever an earlier build had
  // left in the default directory, or failed.
  const directory = mkdtempSync(`${tmpdir()}/prismtty-package-`);
  const fakeBin = `${directory}/bin`;
  const script = `${directory}/package-release.sh`;
  const targetDir = `${directory}/elsewhere`;
  try {
    for (const path of ['bin', 'completions', 'profiles', 'elsewhere/release']) {
      mkdirSync(`${directory}/${path}`, { recursive: true });
    }
    copyFileSync('scripts/package-release.sh', script);
    chmodSync(script, 0o755);
    for (const path of [
      'LICENSE',
      'README.md',
      'completions/prismtty.bash',
      'completions/prismtty.fish',
      'completions/_prismtty',
      'profiles/custom-router.example.yml',
    ]) {
      writeFileSync(`${directory}/${path}`, `${path}\n`);
    }
    for (const binary of ['prismtty', 'ptty', 'ct']) {
      writeFileSync(`${targetDir}/release/${binary}`, `built in the target directory in use\n`);
      chmodSync(`${targetDir}/release/${binary}`, 0o755);
    }
    writeFileSync(
      `${fakeBin}/cargo`,
      `#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  pkgid) printf '%s\n' "path+file:///fixture#1.2.3" ;;
  build) ;;
  *) exit 1 ;;
esac
`,
    );
    chmodSync(`${fakeBin}/cargo`, 0o755);

    const result = spawnSync('bash', [script, 'darwin-aarch64'], {
      cwd: directory,
      env: {
        ...process.env,
        PATH: `${fakeBin}:${process.env.PATH}`,
        SOURCE_DATE_EPOCH: '1700000000',
        CARGO_TARGET_DIR: targetDir,
      },
      encoding: 'utf8',
    });
    assert.equal(result.status, 0, result.stderr || result.stdout);
    const packaged = spawnSync(
      'tar',
      ['-xzOf', `${directory}/dist/prismtty-1.2.3-darwin-aarch64.tar.gz`, 'prismtty-1.2.3-darwin-aarch64/prismtty'],
      { encoding: 'utf8' },
    );
    assert.equal(packaged.status, 0, packaged.stderr);
    assert.equal(packaged.stdout, 'built in the target directory in use\n');
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

// The first bytes of an executable name its object format and its CPU. These
// are enough of a header for the packaging script to tell the targets apart.
const executableHeaders = {
  'darwin-aarch64': Buffer.from([
    0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
  ]),
  'darwin-x86_64': Buffer.from([
    0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00,
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
  ]),
  'linux-x86_64': Buffer.from([
    0x7f, 0x45, 0x4c, 0x46, 0x02, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x3e, 0x00, 0x01, 0x00, 0x00, 0x00,
  ]),
};

// Packages a fixture whose three binaries carry the given headers under the
// given target label, and reports how the packaging script ended.
function packageExecutables(target, builtFor) {
  const directory = mkdtempSync(`${tmpdir()}/prismtty-package-`);
  const fakeBin = `${directory}/bin`;
  const script = `${directory}/package-release.sh`;
  try {
    for (const path of ['bin', 'completions', 'profiles', 'target/release']) {
      mkdirSync(`${directory}/${path}`, { recursive: true });
    }
    copyFileSync('scripts/package-release.sh', script);
    chmodSync(script, 0o755);
    for (const path of [
      'LICENSE',
      'README.md',
      'completions/prismtty.bash',
      'completions/prismtty.fish',
      'completions/_prismtty',
      'profiles/custom-router.example.yml',
    ]) {
      writeFileSync(`${directory}/${path}`, `${path}\n`);
    }
    for (const [binary, builtTarget] of Object.entries(builtFor)) {
      writeFileSync(`${directory}/target/release/${binary}`, executableHeaders[builtTarget]);
      chmodSync(`${directory}/target/release/${binary}`, 0o755);
    }
    writeFileSync(
      `${fakeBin}/cargo`,
      `#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  pkgid) printf '%s\n' "path+file:///fixture#1.2.3" ;;
  build) ;;
  *) exit 1 ;;
esac
`,
    );
    chmodSync(`${fakeBin}/cargo`, 0o755);
    const { CARGO_TARGET_DIR: _inherited, ...inheritedEnv } = process.env;
    const result = spawnSync('bash', [script, target], {
      cwd: directory,
      env: {
        ...inheritedEnv,
        PATH: `${fakeBin}:${process.env.PATH}`,
        SOURCE_DATE_EPOCH: '1700000000',
      },
      encoding: 'utf8',
    });
    const archive = `dist/prismtty-1.2.3-${target}.tar.gz`;
    const listing = spawnSync('tar', ['-tzf', archive], { cwd: directory, encoding: 'utf8' });
    return {
      status: result.status,
      stderr: result.stderr,
      wroteArchive: listing.status === 0,
      packagedAnything: readdirSync(directory).includes('dist'),
    };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

test('release packaging accepts binaries built for the target it is asked to package', () => {
  for (const target of ['darwin-aarch64', 'darwin-x86_64', 'linux-x86_64']) {
    const result = packageExecutables(target, { prismtty: target, ptty: target, ct: target });
    assert.equal(result.status, 0, `${target}: ${result.stderr}`);
    assert.ok(result.wroteArchive, `${target}: no archive was written`);
  }
});

test('release packaging refuses a binary built for another target and names it', () => {
  // The script used to trust its label: it packaged whatever sat in the target
  // directory, so an arm64 macOS binary could ship inside the Linux archive.
  const result = packageExecutables('linux-x86_64', {
    prismtty: 'linux-x86_64',
    ptty: 'linux-x86_64',
    ct: 'darwin-aarch64',
  });
  assert.notEqual(result.status, 0, 'a macOS arm64 ct was packaged as linux-x86_64');
  assert.match(result.stderr, /target\/release\/ct is Mach-O arm64/);
  assert.match(result.stderr, /linux-x86_64 needs ELF x86_64/);
  assert.doesNotMatch(result.stderr, /target\/release\/(?:prismtty|ptty) is/);
  assert.ok(!result.packagedAnything, 'a refused package still staged files under dist/');
});

test('release packaging tells the two macOS CPUs apart', () => {
  const everyBinary = (target) => ({ prismtty: target, ptty: target, ct: target });

  let result = packageExecutables('darwin-aarch64', everyBinary('darwin-x86_64'));
  assert.notEqual(result.status, 0, 'x86_64 binaries were packaged as darwin-aarch64');
  assert.match(result.stderr, /target\/release\/prismtty is Mach-O x86_64/);
  assert.match(result.stderr, /darwin-aarch64 needs Mach-O arm64/);
  assert.ok(!result.packagedAnything);

  result = packageExecutables('darwin-x86_64', everyBinary('darwin-aarch64'));
  assert.notEqual(result.status, 0, 'arm64 binaries were packaged as darwin-x86_64');
  assert.match(result.stderr, /target\/release\/prismtty is Mach-O arm64/);
  assert.match(result.stderr, /darwin-x86_64 needs Mach-O x86_64/);
  assert.ok(!result.packagedAnything);
});

// Files that carry a recognizable executable header, some with the right CPU
// number, without being something the target can run.
Object.assign(executableHeaders, {
  'linux-x86_64 non-PIE, GNU/Linux ABI': Buffer.from('7f454c4602010103000000000000000002003e00', 'hex'),
  'FreeBSD x86_64': Buffer.from('7f454c4602010109000000000000000002003e00', 'hex'),
  'x32 (32-bit ELF for x86_64)': Buffer.from('7f454c4601010100000000000000000002003e00', 'hex'),
  'big-endian ELF': Buffer.from('7f454c460202010000000000000000000002003e', 'hex'),
  'truncated ELF': Buffer.from('7f454c4602', 'hex'),
  'big-endian 64-bit Mach-O': Buffer.from('feedfacf010000120000000000000002', 'hex'),
  '32-bit Mach-O': Buffer.from('cefaedfe070000000300000002000000', 'hex'),
  'big-endian 32-bit Mach-O': Buffer.from('feedface000000120000000000000002', 'hex'),
});

test('release packaging accepts both kinds of Linux x86_64 executable', () => {
  // The position-independent kind is what cargo builds; a fixed-address one
  // marked with the GNU/Linux ABI is the same target.
  for (const kind of ['linux-x86_64', 'linux-x86_64 non-PIE, GNU/Linux ABI']) {
    const result = packageExecutables('linux-x86_64', { prismtty: kind, ptty: kind, ct: kind });
    assert.equal(result.status, 0, `${kind}: ${result.stderr}`);
    assert.ok(result.wroteArchive, `${kind}: no archive was written`);
  }
});

test('release packaging refuses an ELF file that is not a 64-bit Linux executable', () => {
  // FreeBSD and the x32 ABI use the x86_64 machine number too, so the machine
  // number alone let them into the linux-x86_64 archive.
  for (const [kind, said] of [
    ['FreeBSD x86_64', /target\/release\/ct is ELF x86_64 \(OS\/ABI 9\)/],
    ['x32 (32-bit ELF for x86_64)', /target\/release\/ct is ELF x86_64 \(not 64-bit\)/],
    ['big-endian ELF', /target\/release\/ct is ELF x86_64 \(big-endian\)/],
    ['truncated ELF', /target\/release\/ct is ELF \(truncated\)/],
  ]) {
    const result = packageExecutables('linux-x86_64', {
      prismtty: 'linux-x86_64',
      ptty: 'linux-x86_64',
      ct: kind,
    });
    assert.notEqual(result.status, 0, `${kind} was packaged as linux-x86_64`);
    assert.match(result.stderr, said, kind);
    assert.match(result.stderr, /linux-x86_64 needs ELF x86_64$/m, kind);
    assert.ok(!result.packagedAnything, kind);
  }
});

test('release packaging refuses every Mach-O file that is not the 64-bit kind for its CPU', () => {
  // Older and big-endian Mach-O files have other magic numbers. They used to
  // count as "neither format" and were packaged under any label.
  for (const kind of ['big-endian 64-bit Mach-O', '32-bit Mach-O', 'big-endian 32-bit Mach-O']) {
    for (const target of ['linux-x86_64', 'darwin-aarch64', 'darwin-x86_64']) {
      const result = packageExecutables(target, { prismtty: kind, ptty: kind, ct: kind });
      assert.notEqual(result.status, 0, `${kind} was packaged as ${target}`);
      assert.match(result.stderr, /target\/release\/prismtty is Mach-O /, `${kind} as ${target}`);
      assert.ok(!result.packagedAnything, `${kind} as ${target}`);
    }
  }
});

// A Mach-O file as far as the loader reads it to find the program's
// libraries: the header and one load command per library.
function machOLoading(target, libraries) {
  const commands = libraries.map((library) => {
    const command = Buffer.alloc(24 + 8 * Math.ceil((library.length + 1) / 8));
    command.writeUInt32LE(0xc, 0);
    command.writeUInt32LE(command.length, 4);
    command.writeUInt32LE(24, 8);
    command.write(library, 24, 'latin1');
    return command;
  });
  const header = Buffer.alloc(32);
  executableHeaders[target].copy(header, 0, 0, 16);
  header.writeUInt32LE(commands.length, 16);
  header.writeUInt32LE(commands.reduce((size, command) => size + command.length, 0), 20);
  return Buffer.concat([header, ...commands]);
}

// The same for an ELF file: the program headers, the dynamic section and the
// string table its DT_NEEDED entries point into. `dynamic: false` leaves the
// dynamic section out, as in a statically linked program.
function elfLoading(libraries, { dynamic = true, kind = 'linux-x86_64' } = {}) {
  const strings = [Buffer.from([0])];
  let stringsLength = 1;
  const entry = (tag, value) => {
    const bytes = Buffer.alloc(16);
    bytes.writeBigInt64LE(BigInt(tag), 0);
    bytes.writeBigUInt64LE(BigInt(value), 8);
    return bytes;
  };
  const needed = libraries.map((library) => {
    const offset = stringsLength;
    strings.push(Buffer.from(`${library}\0`, 'latin1'));
    stringsLength += library.length + 1;
    return entry(1, offset);
  });
  const segmentCount = dynamic ? 2 : 1;
  const dynamicOffset = 64 + 56 * segmentCount;
  const dynamicSize = dynamic ? 16 * (needed.length + 2) : 0;
  const stringsOffset = dynamicOffset + dynamicSize;
  const segment = (type, offset, size) => {
    const bytes = Buffer.alloc(56);
    bytes.writeUInt32LE(type, 0);
    bytes.writeBigUInt64LE(BigInt(offset), 8);
    bytes.writeBigUInt64LE(BigInt(offset), 16);
    bytes.writeBigUInt64LE(BigInt(size), 32);
    return bytes;
  };
  const header = Buffer.alloc(64);
  executableHeaders[kind].copy(header);
  header.writeBigUInt64LE(64n, 0x20);
  header.writeUInt16LE(56, 0x36);
  header.writeUInt16LE(segmentCount, 0x38);
  return Buffer.concat([
    header,
    segment(1, 0, stringsOffset + stringsLength),
    ...(dynamic ? [segment(2, dynamicOffset, dynamicSize)] : []),
    ...(dynamic ? [...needed, entry(5, stringsOffset), entry(0, 0)] : []),
    ...strings,
  ]);
}

const systemLibraries = {
  'darwin-aarch64': ['/usr/lib/libiconv.2.dylib', '/usr/lib/libSystem.B.dylib'],
  'darwin-x86_64': ['/usr/lib/libiconv.2.dylib', '/usr/lib/libSystem.B.dylib'],
  'linux-x86_64': ['libgcc_s.so.1', 'libc.so.6'],
};
const pcre2Library = {
  'darwin-aarch64': '/opt/homebrew/opt/pcre2/lib/libpcre2-8.0.dylib',
  'darwin-x86_64': '/usr/local/opt/pcre2/lib/libpcre2-8.0.dylib',
  'linux-x86_64': 'libpcre2-8.so.0',
};
const executableLoading = (target, libraries, options) =>
  target === 'linux-x86_64' ? elfLoading(libraries, options) : machOLoading(target, libraries);

test('release packaging accepts binaries that load PCRE2 from the system', () => {
  for (const target of ['darwin-aarch64', 'darwin-x86_64', 'linux-x86_64']) {
    const kind = `${target} loading PCRE2`;
    executableHeaders[kind] = executableLoading(target, [pcre2Library[target], ...systemLibraries[target]]);
    const result = packageExecutables(target, { prismtty: kind, ptty: kind, ct: kind });
    assert.equal(result.status, 0, `${target}: ${result.stderr}`);
    assert.ok(result.wroteArchive, `${target}: no archive was written`);
  }
});

test('release packaging refuses a binary with PCRE2 built into it and names it', () => {
  // When pkg-config does not find libpcre2-8, or PCRE2_SYS_STATIC is set, the
  // build compiles the PCRE2 bundled with pcre2-sys into the binaries without
  // a word. They pass every other check and never get a system PCRE2 update.
  for (const target of ['darwin-aarch64', 'darwin-x86_64', 'linux-x86_64']) {
    const dynamic = `${target} loading PCRE2`;
    const bundled = `${target} with PCRE2 built in`;
    executableHeaders[dynamic] = executableLoading(target, [pcre2Library[target], ...systemLibraries[target]]);
    executableHeaders[bundled] = executableLoading(target, systemLibraries[target]);
    const result = packageExecutables(target, { prismtty: dynamic, ptty: dynamic, ct: bundled });
    assert.notEqual(result.status, 0, `${target}: a ct with PCRE2 built in was packaged`);
    assert.match(result.stderr, /target\/release\/ct does not load libpcre2-8/, target);
    assert.doesNotMatch(result.stderr, /target\/release\/(?:prismtty|ptty) does not load/, target);
    assert.ok(!result.packagedAnything, `${target}: a refused package still staged files under dist/`);
  }
});

test('release packaging refuses a PCRE2 that is not the one the README names', () => {
  // Another library whose name only starts the same way, and on Linux another
  // version of the library than the libpcre2-8.so.0 the README promises.
  for (const [target, library] of [
    ['darwin-aarch64', '/opt/homebrew/opt/pcre2/lib/libpcre2-16.0.dylib'],
    ['darwin-aarch64', '/opt/homebrew/opt/pcre2/lib/libpcre2-8-extra.dylib'],
    ['linux-x86_64', 'libpcre2-8.so.1'],
    ['linux-x86_64', 'libpcre2-16.so.0'],
  ]) {
    const kind = `${target} loading ${library}`;
    executableHeaders[kind] = executableLoading(target, [library, ...systemLibraries[target]]);
    const result = packageExecutables(target, { prismtty: kind, ptty: kind, ct: kind });
    assert.notEqual(result.status, 0, `${library} was accepted for ${target}`);
    assert.match(result.stderr, /target\/release\/prismtty does not load libpcre2-8/, library);
  }
});

test('release packaging refuses an executable whose libraries it cannot read', () => {
  const linux = 'linux-x86_64';
  const loadingPcre2 = [pcre2Library[linux], ...systemLibraries[linux]];
  const cutShort = elfLoading(loadingPcre2);
  const zeroSizedCommand = machOLoading('darwin-aarch64', [pcre2Library['darwin-aarch64']]);
  zeroSizedCommand.writeUInt32LE(0, 36);
  for (const [target, contents, said] of [
    // A statically linked program has no dynamic section at all.
    [linux, elfLoading(loadingPcre2, { dynamic: false }), /target\/release\/prismtty: cannot read the libraries it loads: no dynamic section/],
    [linux, cutShort.subarray(0, cutShort.length - 20), /target\/release\/prismtty: cannot read the libraries it loads/],
    // A whole header whose tables are missing.
    [linux, cutShort.subarray(0, 64), /target\/release\/prismtty: cannot read the libraries it loads/],
    ['darwin-aarch64', machOLoading('darwin-aarch64', [pcre2Library['darwin-aarch64']]).subarray(0, 32), /target\/release\/prismtty: cannot read the libraries it loads/],
    ['darwin-aarch64', zeroSizedCommand, /target\/release\/prismtty: cannot read the libraries it loads/],
  ]) {
    const kind = `unreadable ${target} ${said}`;
    executableHeaders[kind] = contents;
    const result = packageExecutables(target, { prismtty: kind, ptty: kind, ct: kind });
    assert.notEqual(result.status, 0, `${said} was packaged`);
    assert.match(result.stderr, said);
    assert.ok(!result.packagedAnything);
  }
});

test('release packaging leaves a file that ends inside its header to the other checks', () => {
  // Such a file cannot run, so it has no libraries to answer for. The header
  // is the first 64 bytes of an ELF file and the first 32 of a Mach-O file.
  for (const [target, length] of [
    ['linux-x86_64', 63],
    ['darwin-aarch64', 31],
    ['darwin-x86_64', 31],
  ]) {
    const kind = `${target} cut off inside its header`;
    executableHeaders[kind] = executableLoading(target, systemLibraries[target]).subarray(0, length);
    const result = packageExecutables(target, { prismtty: kind, ptty: kind, ct: kind });
    assert.equal(result.status, 0, `${target}: ${result.stderr}`);
    assert.ok(result.wroteArchive, `${target}: no archive was written`);
  }
});
