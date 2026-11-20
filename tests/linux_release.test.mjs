import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  chmodSync,
  copyFileSync,
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

// Builds the part of an ELF file the dynamic loader reads before it starts a
// program: for each shared library, the symbol versions the program needs.
function elfNeeding(versionsByLibrary) {
  const strings = [Buffer.from([0])];
  let stringsLength = 1;
  const offsetOf = (text) => {
    const offset = stringsLength;
    strings.push(Buffer.from(`${text}\0`, 'latin1'));
    stringsLength += text.length + 1;
    return offset;
  };

  const needs = [];
  const libraries = Object.entries(versionsByLibrary);
  libraries.forEach(([library, versions], libraryIndex) => {
    const need = Buffer.alloc(16);
    need.writeUInt16LE(1, 0);
    need.writeUInt16LE(versions.length, 2);
    need.writeUInt32LE(offsetOf(library), 4);
    need.writeUInt32LE(16, 8);
    need.writeUInt32LE(libraryIndex === libraries.length - 1 ? 0 : 16 + 16 * versions.length, 12);
    needs.push(need);
    versions.forEach((version, versionIndex) => {
      const aux = Buffer.alloc(16);
      aux.writeUInt16LE(versionIndex + 2, 6);
      aux.writeUInt32LE(offsetOf(version), 8);
      aux.writeUInt32LE(versionIndex === versions.length - 1 ? 0 : 16, 12);
      needs.push(aux);
    });
  });
  const stringTable = Buffer.concat(strings);
  const versionNeeds = Buffer.concat(needs);

  const sectionHeader = (type, offset, size, link, info) => {
    const header = Buffer.alloc(64);
    header.writeUInt32LE(type, 4);
    header.writeBigUInt64LE(BigInt(offset), 24);
    header.writeBigUInt64LE(BigInt(size), 32);
    header.writeUInt32LE(link, 40);
    header.writeUInt32LE(info, 44);
    return header;
  };
  const stringTableOffset = 64;
  const versionNeedsOffset = stringTableOffset + stringTable.length;
  const sectionTableOffset = versionNeedsOffset + versionNeeds.length;

  const header = Buffer.alloc(64);
  Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1]).copy(header);
  header.writeUInt16LE(3, 16);
  header.writeUInt16LE(62, 18);
  header.writeBigUInt64LE(BigInt(sectionTableOffset), 0x28);
  header.writeUInt16LE(64, 0x3a);
  header.writeUInt16LE(3, 0x3c);

  return Buffer.concat([
    header,
    stringTable,
    versionNeeds,
    sectionHeader(0, 0, 0, 0, 0),
    sectionHeader(3, stringTableOffset, stringTable.length, 0, 0),
    sectionHeader(0x6ffffffe, versionNeedsOffset, versionNeeds.length, 1, libraries.length),
  ]);
}

const builtOnTheBaseline = elfNeeding({
  'libgcc_s.so.1': ['GCC_3.0', 'GCC_4.2.0'],
  'libc.so.6': ['GLIBC_2.2.5', 'GLIBC_2.34', 'GLIBC_2.17', 'GLIBC_2.9'],
});
const builtOnANewerSystem = elfNeeding({
  'libgcc_s.so.1': ['GCC_3.0'],
  'libc.so.6': ['GLIBC_2.2.5', 'GLIBC_2.39', 'GLIBC_2.34'],
});

function checkFloor(floor, files) {
  const directory = mkdtempSync(`${tmpdir()}/prismtty-floor-`);
  try {
    const paths = Object.entries(files).map(([name, contents]) => {
      writeFileSync(`${directory}/${name}`, contents);
      return `${directory}/${name}`;
    });
    return spawnSync('bash', ['scripts/check-glibc-floor.sh', floor, ...paths], { encoding: 'utf8' });
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

test('the glibc floor check accepts an executable that needs the floor or less', () => {
  const result = checkFloor('2.34', { prismtty: builtOnTheBaseline, ptty: builtOnTheBaseline });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /prismtty: needs glibc 2\.34, floor 2\.34/);

  // Versions compare as numbers: 2.9 is older than 2.34.
  assert.equal(checkFloor('2.35', { prismtty: builtOnTheBaseline }).status, 0);
  assert.notEqual(checkFloor('2.9', { prismtty: builtOnTheBaseline }).status, 0);
});

test('the glibc floor check fails when an executable needs a newer glibc', () => {
  // The binaries in the 1.3.7 archive require GLIBC_2.39, the glibc of the
  // runner they were linked on. Nothing measured that, so nothing failed.
  const result = checkFloor('2.34', {
    prismtty: builtOnTheBaseline,
    ptty: builtOnTheBaseline,
    ct: builtOnANewerSystem,
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /\/ct needs glibc 2\.39, above the floor 2\.34/);
  assert.doesNotMatch(result.stderr, /\/(?:prismtty|ptty) needs glibc 2\.39/);
});

test('the glibc floor check fails closed on anything it cannot measure', () => {
  let result = checkFloor('2.34', { prismtty: Buffer.from('#!/bin/sh\n') });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /prismtty: cannot read its glibc requirement/);

  result = checkFloor('2.34', { prismtty: elfNeeding({ 'libgcc_s.so.1': ['GCC_3.0'] }) });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /prismtty: no glibc requirement found/);

  // A requirement that is not a plain version, such as the marker newer
  // linkers add for packed relocations, is one an older glibc cannot meet.
  result = checkFloor('2.34', {
    prismtty: elfNeeding({ 'libc.so.6': ['GLIBC_2.34', 'GLIBC_ABI_DT_RELR'] }),
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /prismtty: cannot read its glibc requirement: .*GLIBC_ABI_DT_RELR/);

  assert.equal(spawnSync('bash', ['scripts/check-glibc-floor.sh', '2.34']).status, 2);
});

const linuxScript = 'scripts/package-linux-release.sh';
const scriptValue = (name) => {
  const match = read(linuxScript).match(new RegExp(`^${name}="([^"]+)"$`, 'm'));
  assert.ok(match, `${linuxScript} sets ${name}`);
  return match[1];
};

// Runs the Linux packaging script with stand-ins for the host tools it calls.
// The stand-in for docker records each call and plays the build container by
// writing the staged binaries the real one would leave behind.
function packageForLinux({ binaries, rustc = 'rustc 1.85.0 (fixture)', smokeStatus = '0' }) {
  const directory = mkdtempSync(`${tmpdir()}/prismtty-linux-package-`);
  const fakeBin = `${directory}/bin`;
  try {
    mkdirSync(fakeBin);
    mkdirSync(`${directory}/scripts`);
    for (const script of ['package-linux-release.sh', 'check-glibc-floor.sh']) {
      copyFileSync(`scripts/${script}`, `${directory}/scripts/${script}`);
    }
    writeFileSync(`${directory}/Cargo.toml`, '[package]\nname = "prismtty"\nversion = "1.2.3"\nrust-version = "1.85"\n');
    for (const [name, contents] of Object.entries(binaries)) {
      writeFileSync(`${directory}/built-${name}`, contents);
    }
    const tools = {
      uname: 'case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; esac',
      rustc: 'case "$1" in --version) echo "$FAKE_RUSTC" ;; --print) echo /fixture/toolchain ;; esac',
      timeout: 'shift\nexec "$@"',
      docker: `call="$*"
printf '%s\\n' "\${call//$'\\n'/ }" >> docker.log
case "$1" in
  pull) ;;
  run)
    if [[ " $* " == *" --network none "* ]]; then
      exit "$FAKE_SMOKE_STATUS"
    fi
    mkdir -p dist/prismtty-1.2.3-linux-x86_64
    for name in prismtty ptty ct; do
      cp "built-$name" "dist/prismtty-1.2.3-linux-x86_64/$name"
    done
    ;;
esac`,
    };
    for (const [name, body] of Object.entries(tools)) {
      writeFileSync(`${fakeBin}/${name}`, `#!/usr/bin/env bash\nset -euo pipefail\n${body}\n`);
      chmodSync(`${fakeBin}/${name}`, 0o755);
    }
    const result = spawnSync('bash', ['scripts/package-linux-release.sh'], {
      cwd: directory,
      env: {
        ...process.env,
        PATH: `${fakeBin}:${process.env.PATH}`,
        SOURCE_DATE_EPOCH: '1700000000',
        FAKE_RUSTC: rustc,
        FAKE_SMOKE_STATUS: smokeStatus,
      },
      encoding: 'utf8',
    });
    const log = existsSync(`${directory}/docker.log`) ? read(`${directory}/docker.log`) : '';
    return { status: result.status, stderr: result.stderr, calls: log.split('\n').filter(Boolean) };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

const everyBinary = (contents) => ({ prismtty: contents, ptty: contents, ct: contents });

test('the Linux archive is built, measured and smoke-tested in one pinned image', () => {
  const image = scriptValue('image');
  assert.match(image, /^docker\.io\/library\/[a-z0-9]+@sha256:[0-9a-f]{64}$/, 'the base image is pinned by digest');

  const result = packageForLinux({ binaries: everyBinary(builtOnTheBaseline) });
  assert.equal(result.status, 0, result.stderr);
  const [pull, build, smoke, ...rest] = result.calls;
  assert.deepEqual(rest, []);
  assert.equal(pull, `pull --platform linux/amd64 ${image}`);
  assert.match(build, /^run /);
  assert.ok(build.includes(` ${image} `), 'the build runs in the pinned image');
  assert.match(build, / --env SOURCE_DATE_EPOCH=1700000000 /);
  assert.match(build, / --volume \/fixture\/toolchain:\/opt\/rust:ro /);
  assert.match(build, /scripts\/package-release\.sh "\$\{TARGET_NAME\}"/);
  // Each download is bounded and retried, and the build runs without network.
  assert.match(build, /fetch dnf .*--setopt=timeout=\d+ --setopt=retries=\d+ install /);
  assert.ok(
    build.indexOf('fetch cargo fetch --locked') < build.indexOf('export CARGO_NET_OFFLINE=true')
      && build.indexOf('export CARGO_NET_OFFLINE=true') < build.indexOf('scripts/package-release.sh'),
    'the crates are fetched before an offline build',
  );
  // The smoke test starts from the bare image, without the network, so it
  // only passes when a stock system of that age can run the binaries.
  assert.match(smoke, /^run /);
  assert.ok(smoke.includes(` ${image} `), 'the smoke test runs in the pinned image');
  assert.match(smoke, / --network none /);
  for (const proof of ['--version', '--profile generic', 'test -t 1 && exit 7']) {
    assert.ok(smoke.includes(proof), `the smoke test runs ${proof}`);
  }
});

test('the Linux archive is refused when a binary needs more than the glibc floor', () => {
  const result = packageForLinux({
    binaries: { ...everyBinary(builtOnTheBaseline), ptty: builtOnANewerSystem },
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /ptty needs glibc 2\.39, above the floor/);
  assert.ok(!result.calls.some((call) => call.includes('--network none')), 'the smoke test did not run');
});

test('the Linux archive is refused when the smoke test fails', () => {
  const result = packageForLinux({ binaries: everyBinary(builtOnTheBaseline), smokeStatus: '1' });
  assert.notEqual(result.status, 0);
});

test('the Linux archive is only built with the Rust version the release uses', () => {
  const result = packageForLinux({
    binaries: everyBinary(builtOnTheBaseline),
    rustc: 'rustc 1.90.0 (fixture)',
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /1\.85\.0/);
  assert.deepEqual(result.calls, [], 'nothing was pulled or built');
});

test('pull requests and releases build the Linux archive with the same script', () => {
  const command = 'run: bash scripts/package-linux-release.sh';
  const release = read('.github/workflows/release.yml');
  const ci = read('.github/workflows/ci.yml');

  // The release only calls the script; everything it does was already run by
  // the pull request that changed it.
  assert.match(
    release,
    new RegExp(`\\n        if: matrix\\.target_name == 'linux-x86_64'\\n(?: {8}.*\\n)*? {8}${command}\\n`),
  );
  assert.match(
    release,
    /\n        if: matrix\.target_name != 'linux-x86_64'\n        run: bash scripts\/package-release\.sh "\$\{\{ matrix\.target_name \}\}"\n/,
  );
  assert.equal(release.split('scripts/package-linux-release.sh').length - 1, 1);

  const jobStart = ci.indexOf('\n  linux-release:\n');
  assert.notEqual(jobStart, -1, 'ci.yml has a linux-release job');
  const job = ci.slice(jobStart);
  assert.match(job, /runs-on: ubuntu-24\.04\n/);
  assert.match(job, /toolchain: 1\.85\.0\n/);
  assert.ok(job.includes(`        ${command}\n`));
});

test('the README states the glibc floor the Linux archive is held to', () => {
  const floor = scriptValue('glibc_floor');
  assert.match(floor, /^\d+\.\d+$/);
  const readme = read('README.md').replace(/\s+/g, ' ');
  assert.deepEqual(
    [...readme.matchAll(/glibc (\d+\.\d+)/g)].map((match) => match[1]),
    [floor],
    'README.md names the glibc baseline once, and it is the floor the script enforces',
  );
  for (const library of ['libpcre2-8.so.0', 'libgcc_s.so.1']) {
    assert.ok(readme.includes(library), `README.md names ${library}`);
  }
});
