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
// program: for each shared library, the symbol versions the program needs,
// and optionally the note that says which x86-64 level the CPU must have.
function elfNeeding(versionsByLibrary, { isaNeeded, programHeaders = true } = {}) {
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
  // One GNU property note: "x86 ISA needed", a bit per x86-64 level.
  const property = Buffer.alloc(isaNeeded === undefined ? 0 : 32);
  if (isaNeeded !== undefined) {
    property.writeUInt32LE(4, 0);
    property.writeUInt32LE(16, 4);
    property.writeUInt32LE(5, 8);
    property.write('GNU\0', 12, 'latin1');
    property.writeUInt32LE(0xc0008002, 16);
    property.writeUInt32LE(4, 20);
    property.writeUInt32LE(isaNeeded, 24);
  }
  const segmentCount = programHeaders ? (isaNeeded === undefined ? 1 : 2) : 0;
  const stringTableOffset = 64 + 56 * segmentCount;
  const versionNeedsOffset = stringTableOffset + stringTable.length;
  const padding = Buffer.alloc((8 - ((versionNeedsOffset + versionNeeds.length) % 8)) % 8);
  const propertyOffset = versionNeedsOffset + versionNeeds.length + padding.length;
  const sectionTableOffset = propertyOffset + property.length;

  const programHeader = (type, offset, size) => {
    const segment = Buffer.alloc(56);
    segment.writeUInt32LE(type, 0);
    segment.writeBigUInt64LE(BigInt(offset), 8);
    segment.writeBigUInt64LE(BigInt(size), 32);
    return segment;
  };
  const segments = [
    programHeader(1, 0, sectionTableOffset),
    programHeader(0x6474e553, propertyOffset, property.length),
  ].slice(0, segmentCount);

  const header = Buffer.alloc(64);
  Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1]).copy(header);
  header.writeUInt16LE(3, 16);
  header.writeUInt16LE(62, 18);
  header.writeBigUInt64LE(BigInt(segmentCount ? 64 : 0), 0x20);
  header.writeBigUInt64LE(BigInt(sectionTableOffset), 0x28);
  header.writeUInt16LE(56, 0x36);
  header.writeUInt16LE(segmentCount, 0x38);
  header.writeUInt16LE(64, 0x3a);
  header.writeUInt16LE(3, 0x3c);

  return Buffer.concat([
    header,
    ...segments,
    stringTable,
    versionNeeds,
    padding,
    property,
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

test('the check fails when an executable needs more than a baseline x86-64 CPU', () => {
  // A distribution built for x86-64-v2 marks what it links, and the loader of
  // glibc 2.33 and later refuses to start a marked program on a CPU or a
  // virtual machine that only offers the baseline.
  const versions = { 'libc.so.6': ['GLIBC_2.2.5', 'GLIBC_2.34'] };
  let result = checkFloor('2.34', {
    prismtty: elfNeeding(versions, { isaNeeded: 0b0001 }),
    ptty: elfNeeding(versions),
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /\/prismtty: x86 ISA needed: baseline$/m);
  assert.match(result.stdout, /\/ptty: x86 ISA needed: none stated$/m);

  result = checkFloor('2.34', {
    prismtty: elfNeeding(versions, { isaNeeded: 0b0001 }),
    ct: elfNeeding(versions, { isaNeeded: 0b0011 }),
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /\/ct needs x86-64-v2, above baseline x86-64$/m);
  assert.doesNotMatch(result.stderr, /prismtty needs x86-64/);

  result = checkFloor('2.34', { ct: elfNeeding(versions, { isaNeeded: 0b1110 }) });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /\/ct needs x86-64-v2, x86-64-v3, x86-64-v4, above baseline x86-64$/m);
});

test('the check fails closed on a file with nothing for the loader to map', () => {
  const result = checkFloor('2.34', {
    prismtty: elfNeeding({ 'libc.so.6': ['GLIBC_2.34'] }, { programHeaders: false }),
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /prismtty: cannot read its CPU requirement: no program headers/);
});

const linuxScript = 'scripts/package-linux-release.sh';
const scriptValue = (name) => {
  const match = read(linuxScript).match(new RegExp(`^${name}="([^"]+)"$`, 'm'));
  assert.ok(match, `${linuxScript} sets ${name}`);
  return match[1];
};

// Runs the Linux packaging script with stand-ins for the host tools it calls.
// The stand-in for docker records each call and plays the build container by
// leaving behind what the real one does: the staged binaries, the tarball and
// its checksum file. `archived` and `notExecutable` make the tarball differ
// from the staged binaries.
function packageForLinux({
  binaries,
  archived = binaries,
  notExecutable = [],
  staleChecksum = false,
  rustc = 'rustc 1.85.0 (fixture)',
  smokeStatus = '0',
  standIn = '',
}) {
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
    mkdirSync(`${directory}/archive/prismtty-1.2.3-linux-x86_64`, { recursive: true });
    for (const [name, contents] of Object.entries(archived)) {
      const member = `${directory}/archive/prismtty-1.2.3-linux-x86_64/${name}`;
      writeFileSync(member, contents);
      chmodSync(member, notExecutable.includes(name) ? 0o644 : 0o755);
    }
    const tools = {
      uname: 'case "$1" in -s) echo Linux ;; -m) echo x86_64 ;; esac',
      rustc: 'case "$1" in --version) echo "$FAKE_RUSTC" ;; --print) echo /fixture/toolchain ;; esac',
      timeout: 'while [[ "$1" == -* ]]; do shift; done\nshift\nexec "$@"',
      docker: `call="$*"
printf '%s\\n' "\${call//$'\\n'/ }" >> docker.log
case "$1" in
  pull) ;;
  run)
    if [[ " $* " == *" --network none "* ]]; then
      if [[ -z "$FAKE_STAND_IN" ]]; then
        exit "$FAKE_SMOKE_STATUS"
      fi
      mkdir -p package
      for name in prismtty ptty ct; do
        printf '#!/usr/bin/env bash\n%s\n' "$FAKE_STAND_IN" > "package/$name"
        chmod +x "package/$name"
      done
      smoke="\${@: -1}"
      status=0
      VERSION=1.2.3 bash -c "\${smoke//\\/package\\//$PWD/package/}" || status=$?
      exit "$status"
    fi
    mkdir -p dist/prismtty-1.2.3-linux-x86_64
    for name in prismtty ptty ct; do
      cp "built-$name" "dist/prismtty-1.2.3-linux-x86_64/$name"
    done
    tar -czf dist/prismtty-1.2.3-linux-x86_64.tar.gz -C archive prismtty-1.2.3-linux-x86_64
    (cd dist && shasum -a 256 prismtty-1.2.3-linux-x86_64.tar.gz > prismtty-1.2.3-linux-x86_64.tar.gz.sha256)
    if [[ -n "$FAKE_STALE_CHECKSUM" ]]; then
      printf 'changed after the checksum was written' >> dist/prismtty-1.2.3-linux-x86_64.tar.gz
    fi
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
        FAKE_STALE_CHECKSUM: staleChecksum ? 'yes' : '',
        FAKE_STAND_IN: standIn,
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
  // The packages come from the archive as it was at one moment, so a rebuild
  // of the same commit installs the same compiler and libraries.
  const snapshot = scriptValue('snapshot');
  assert.match(snapshot, /^\d{8}T\d{6}Z$/);
  assert.match(build, new RegExp(` --env SNAPSHOT=${snapshot} `));
  assert.deepEqual(
    [...build.matchAll(/deb (\S+) /g)].map((match) => match[1]),
    Array(3).fill('https://snapshot.ubuntu.com/ubuntu/${SNAPSHOT}'),
    'every package source is the dated snapshot',
  );
  assert.match(build, /cat > \/etc\/apt\/sources\.list </);
  // Each download is bounded and retried, and the build runs without network.
  assert.match(build, /fetch apt-get "\$\{apt_options\[@\]\}" update /);
  assert.match(build, /fetch apt-get "\$\{apt_options\[@\]\}" install /);
  assert.match(build, /-o Acquire::Retries=\d+ /);
  assert.match(build, /-o Acquire::https::Timeout=\d+ /);
  const fetchAt = build.indexOf('fetch cargo fetch --locked --target x86_64-unknown-linux-gnu');
  const offlineAt = build.indexOf('export CARGO_NET_OFFLINE=true');
  const packageAt = build.indexOf('bash scripts/package-release.sh');
  assert.notEqual(fetchAt, -1, 'the crates for the release target are fetched');
  assert.notEqual(offlineAt, -1, 'the build is taken offline');
  assert.notEqual(packageAt, -1, 'the packaging script runs');
  assert.ok(fetchAt < offlineAt && offlineAt < packageAt, 'the crates are fetched before an offline build');
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

test('the build container can read the checkout and write only to dist', () => {
  // Package scripts and cargo build scripts run as root in the container, and
  // the host goes on to run scripts from the same checkout. Mounted read-only,
  // nothing in it can have been changed by then.
  const result = packageForLinux({ binaries: everyBinary(builtOnTheBaseline) });
  assert.equal(result.status, 0, result.stderr);
  const build = result.calls.find((call) => call.startsWith('run ') && !call.includes(' --network none '));
  assert.match(build, / --volume [^ :]+:\/work:ro /);
  assert.match(build, / --volume [^ :]+\/dist:\/work\/dist /);
  assert.equal(
    [...build.matchAll(/ --volume /g)].length,
    3,
    'the checkout, dist and the toolchain are the only volumes',
  );
});

test('the Linux archive is measured and run from the tarball, not from what the build left beside it', () => {
  // The staged copies need a newer glibc here and the archived ones do not:
  // what ships passes.
  let result = packageForLinux({
    binaries: everyBinary(builtOnANewerSystem),
    archived: everyBinary(builtOnTheBaseline),
  });
  assert.equal(result.status, 0, result.stderr);
  const smoke = result.calls.find((call) => call.includes(' --network none '));
  assert.match(smoke, / --volume \S+\/prismtty-1\.2\.3-linux-x86_64:\/package:ro /);
  assert.doesNotMatch(smoke, /\/dist\/prismtty-1\.2\.3-linux-x86_64:\/package/);

  // The other way round, what ships is refused.
  result = packageForLinux({
    binaries: everyBinary(builtOnTheBaseline),
    archived: { ...everyBinary(builtOnTheBaseline), ct: builtOnANewerSystem },
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /\/ct needs glibc 2\.39, above the floor/);
});

test('the Linux archive is refused when a binary is missing from it or lost its execute bit', () => {
  const { ct: _missing, ...withoutCt } = everyBinary(builtOnTheBaseline);
  let result = packageForLinux({ binaries: everyBinary(builtOnTheBaseline), archived: withoutCt });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /prismtty-1\.2\.3-linux-x86_64\.tar\.gz does not hold an executable ct$/m);
  assert.ok(!result.calls.some((call) => call.includes(' --network none ')), 'the smoke test did not run');

  result = packageForLinux({ binaries: everyBinary(builtOnTheBaseline), notExecutable: ['ptty'] });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /prismtty-1\.2\.3-linux-x86_64\.tar\.gz does not hold an executable ptty$/m);
});

test('the Linux archive is refused when it does not match its checksum file', () => {
  const result = packageForLinux({ binaries: everyBinary(builtOnTheBaseline), staleChecksum: true });
  assert.notEqual(result.status, 0);
  assert.ok(!result.calls.some((call) => call.includes(' --network none ')), 'the smoke test did not run');
});

test('every bounded download is killed if it outlives its time limit', () => {
  // Without --kill-after, timeout sends one signal that a stalled program may
  // ignore, and then waits for it.
  const script = read(linuxScript);
  assert.match(script, /timeout --kill-after=\d+ \d+ docker pull /);
  assert.match(script, /timeout --kill-after=\d+ \d+ "\$@"/);
  assert.doesNotMatch(script, /timeout \d/);
});

// A program that answers the smoke test's three questions the way a healthy
// binary does, unless told otherwise.
const smokeStandIn = ({ versionStatus = 0, line = '\\033[38;5;51m192.0.2.1\\033[0m down' } = {}) => `case "$1" in
  --version) echo "prismtty 1.2.3"; exit ${versionStatus} ;;
  --profile) cat > /dev/null; printf '${line}\\n' ;;
  --) exit 7 ;;
esac`;
const smokeTest = (standIn) => packageForLinux({ binaries: everyBinary(builtOnTheBaseline), standIn });

test('the smoke test fails when a binary prints its version and then fails', () => {
  assert.equal(smokeTest(smokeStandIn()).status, 0);

  const result = smokeTest(smokeStandIn({ versionStatus: 3 }));
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /prismtty --version failed or did not print prismtty 1\.2\.3/);
});

test('the smoke test wants a foreground color on the address itself', () => {
  for (const line of [
    '\\033[31m192.0.2.1\\033[0m down',
    '\\033[1;38;2;10;20;30m192.0.2.1\\033[0m down',
    '\\033[48;5;31;92m192.0.2.1\\033[0m down',
    '\\033[48;2;31;32;33;4;38;5;51m192.0.2.1\\033[0m down',
  ]) {
    const result = smokeTest(smokeStandIn({ line }));
    assert.equal(result.status, 0, `${line}: ${result.stderr}`);
  }
  // Some escape sequence in front of the address is not enough: a reset, an
  // attribute or a background color leaves the address uncolored.
  for (const line of [
    '192.0.2.1 down',
    '\\033[0m192.0.2.1 \\033[31mdown\\033[0m',
    '\\033[1m192.0.2.1\\033[0m down',
    '\\033[48;5;31m192.0.2.1\\033[0m down',
    '\\033[48;2;31;32;33m192.0.2.1\\033[0m down',
    '\\033[31m\\033[0m192.0.2.1 down',
  ]) {
    const result = smokeTest(smokeStandIn({ line }));
    assert.notEqual(result.status, 0, `${line} passed as a colored address`);
    assert.match(result.stderr, /prismtty --profile generic failed or did not color the address/, line);
  }
});
