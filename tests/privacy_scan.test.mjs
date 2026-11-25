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

// `git grep -n` prefixes each line with `path:line:`. The IPv6 pass used to
// scan that record whole, so the line number joined the address that began
// the line: a documentation address read one group longer and was rejected.
test('privacy scan reads an IPv6 address at the start of a line on its own', () => {
  const allowed = runPrivacyScan('2001:db8::1 is the documentation router\n');
  assert.equal(allowed.status, 0, allowed.stderr || allowed.stdout);

  const rejected = runPrivacyScan(
    `${['2606', '4700', '4700', '', '1111'].join(':')} is a public resolver\n`,
  );
  assert.notEqual(rejected.status, 0);
  assert.match(rejected.stdout, /IPv6 address outside documentation ranges found/);
});

// Network devices print zero-padded octets. They are the same address.
test('privacy scan rejects a zero-padded private IPv4 address', () => {
  const address = ['10', '001', '2', '3'].join('.');
  const result = runPrivacyScan(`neighbor ${address} up\n`);

  assert.notEqual(result.status, 0);
  assert.match(result.stdout, /IPv4 address outside documentation ranges found/);
});

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

// The tests below read the text chunks of PNG files.
import {
  closeSync,
  constants,
  existsSync,
  openSync,
  readFileSync,
  readdirSync,
  statSync,
  symlinkSync,
} from 'node:fs';
import { deflateSync } from 'node:zlib';

// Runs the scan in a process group of its own and ends the whole group if
// the scan is still running after a minute, so nothing it started is left
// behind. The watchdog must not hold the pipes the test reads.
const boundedScan = `
set -m
bash scripts/privacy-scan.sh &
scan=$!
(sleep 60; kill -KILL -- -"$scan") < /dev/null > /dev/null 2>&1 &
watchdog=$!
status=0
wait "$scan" || status=$?
kill -KILL -- -"$watchdog" 2> /dev/null
times > "$SCAN_TIMES"
exit "$status"
`;

// Runs the scan over several files at once, in the locale given. `links` are
// tracked symbolic links, name to target, and `afterStaging` changes the
// working tree once everything is staged. A scan that was ended for running
// too long reports the status of its kill signal, never 0. `cpuSeconds` is
// the processor time the scan and everything it started used.
function scanFiles(files, { tracked = true, locale = 'en_US.UTF-8', links = {}, afterStaging = () => {} } = {}) {
  const directory = mkdtempSync(`${tmpdir()}/prismtty-privacy-`);
  const times = `${mkdtempSync(`${tmpdir()}/prismtty-privacy-times-`)}/times`;
  try {
    mkdirSync(`${directory}/scripts`);
    copyFileSync(privacyScan, `${directory}/scripts/privacy-scan.sh`);
    execFileSync('git', ['init', '--quiet'], { cwd: directory });
    execFileSync('git', ['add', 'scripts/privacy-scan.sh'], { cwd: directory });
    for (const [path, contents] of Object.entries(files)) {
      mkdirSync(dirname(`${directory}/${path}`), { recursive: true });
      writeFileSync(`${directory}/${path}`, contents);
      if (tracked) {
        execFileSync('git', ['add', path], { cwd: directory });
      }
    }
    for (const [path, target] of Object.entries(links)) {
      symlinkSync(target, `${directory}/${path}`);
      execFileSync('git', ['add', path], { cwd: directory });
    }
    afterStaging(directory);
    const result = spawnSync('bash', ['-c', boundedScan], {
      cwd: directory,
      encoding: 'utf8',
      env: { ...process.env, LC_ALL: locale, SCAN_TIMES: times },
    });
    // The second line of `times` is the user and system time of the
    // processes the shell waited for, as minutes and seconds.
    const spent = [...readFileSync(times, 'utf8').trim().split('\n')[1].matchAll(/(\d+)m([\d.]+)s/g)];
    result.cpuSeconds = spent.reduce((sum, [, minutes, seconds]) => sum + 60 * Number(minutes) + Number(seconds), 0);
    return result;
  } finally {
    rmSync(directory, { recursive: true, force: true });
    rmSync(dirname(times), { recursive: true, force: true });
  }
}

const privateAddress = ['10', '20', '30', '40'].join('.');
const uniqueLocalAddress = ['fd12', '3456', '789a', '', '1'].join(':');
const shortUniqueLocalAddress = ['fd', '', '1'].join(':');
const labHost = ['core1', 'example', 'lab'].join('.');
// Compressed image data spells short IPv6 addresses by chance, and font
// tables spell dotted quads; these are from the site's own images and from
// system fonts. Like that data, it is not valid UTF-8.
const addressLikeNoise = Buffer.concat([
  Buffer.from([0x78, 0xda, 0xed, 0xff]),
  Buffer.from(
    ` ${['408', '', ''].join(':')} ${['F', '', 'ec'].join(':')} ${['9', '', '8'].join(':')} .${Array(4).fill('8').join('.')} `,
  ),
]);

const uint = (bytes, value, littleEndian = false) => {
  const buffer = Buffer.alloc(bytes);
  if (littleEndian) {
    buffer.writeUIntLE(value, 0, bytes);
  } else {
    buffer.writeUIntBE(value, 0, bytes);
  }
  return buffer;
};
const latin1 = (text) => Buffer.from(text, 'latin1');

const crcTable = Array.from({ length: 256 }, (_, index) => {
  let value = index;
  for (let bit = 0; bit < 8; bit += 1) {
    value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1;
  }
  return value >>> 0;
});
const crc32 = (bytes) => {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc = crcTable[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  }
  return (crc ^ 0xffffffff) >>> 0;
};

// A chunk with its checksum. A length given apart from the content is a lie
// the scan has to catch.
const pngChunk = (type, body, length = body.length) =>
  Buffer.concat([uint(4, length), latin1(type), body, uint(4, crc32(Buffer.concat([latin1(type), body])))]);
// An image a decoder accepts: one row of gray pixels, stored without
// compression, so that the pixel data holds the address-like bytes as they
// are. The chunks given go between the header and the pixels.
const png = (...chunks) =>
  Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk('IHDR', Buffer.concat([uint(4, addressLikeNoise.length), uint(4, 1), Buffer.from([8, 0, 0, 0, 0])])),
    ...chunks,
    pngChunk('IDAT', deflateSync(Buffer.concat([Buffer.from([0]), addressLikeNoise]), { level: 0 })),
    pngChunk('IEND', Buffer.alloc(0)),
  ]);
const pngText = (text) => pngChunk('tEXt', latin1(`impeccable:prompt\0${text}`));
const pngCompressedText = (text, stream = deflateSync(text)) =>
  pngChunk('zTXt', Buffer.concat([latin1('impeccable:prompt\0\0'), stream]));
// Keyword, compressed flag, method, empty language and translated keyword.
const pngInternationalText = (text, { compressed = true, stream = deflateSync(text) } = {}) =>
  pngChunk(
    'iTXt',
    Buffer.concat([
      latin1(`impeccable:prompt\0${compressed ? '\x01' : '\0'}\0\0\0`),
      compressed ? stream : Buffer.from(text),
    ]),
  );

const ipv4Found = /IPv4 address outside documentation ranges found/;
const ipv6Found = /IPv6 address outside documentation ranges found/;
const markerFound = /Sensitive real-world capture marker found/;

for (const [label, path, contents, found] of [
  [
    'a PNG text chunk',
    'docs/shot.png',
    png(pngText(`capture of lab switch at ${privateAddress} via ssh`)),
    ipv4Found,
  ],
  ['a compressed PNG text chunk', 'shot.png', png(pngCompressedText(`seen from ${uniqueLocalAddress}`)), ipv6Found],
  [
    'a path in a compressed international PNG text chunk',
    'shot.png',
    png(pngInternationalText(`copied from ${sensitiveFixturePath}`)),
    markerFound,
  ],
  ['a host in a compressed PNG text chunk', 'docs/shot.png', png(pngCompressedText(`ssh ${labHost}`)), markerFound],
  [
    'an international PNG text chunk',
    'shot.png',
    png(pngInternationalText(`café at ${privateAddress}`, { compressed: false })),
    ipv4Found,
  ],
  [
    // Any flag but 0 says the text is compressed to at least one widely
    // used reader, which shows it.
    'an international PNG text chunk with an unknown compression flag',
    'shot.png',
    png(pngChunk('iTXt', Buffer.concat([latin1('note\0\x02\0\0\0'), deflateSync(`router ${privateAddress}`)]))),
    ipv4Found,
  ],
  // Five characters between two NUL bytes: a keyword that ends, an address.
  ['a short IPv6 address in a PNG text chunk', 'shot.png', png(pngText(shortUniqueLocalAddress)), ipv6Found],
  [
    'a short IPv6 address in a compressed PNG text chunk',
    'shot.png',
    png(pngCompressedText(shortUniqueLocalAddress)),
    ipv6Found,
  ],
  [
    'the text behind any number of empty PNG chunks',
    'shot.png',
    png(...Array(64).fill(pngChunk('tEXt', Buffer.alloc(0))), pngText(`router ${privateAddress}`)),
    ipv4Found,
  ],
]) {
  test(`privacy scan reads ${label}`, () => {
    const result = scanFiles({ [path]: contents });

    assert.notEqual(result.status, 0, `${label} passed the scan`);
    assert.match(result.stdout, found);
    // Found in the text of the chunk, with the file named.
    assert.ok(result.stdout.includes(`${path}:embedded:`), result.stdout);
  });
}

test('privacy scan names the PNG file of each path or host it finds', () => {
  // The search numbers the lines of every file in one sequence.
  const result = scanFiles({
    'a/first.png': png(pngCompressedText('synthetic\nnotes\nonly')),
    'b/second.png': png(pngCompressedText(`one\nssh ${labHost}\nthree`)),
    'c/third.png': png(pngCompressedText(`copied from ${sensitiveFixturePath}`)),
  });

  assert.notEqual(result.status, 0);
  assert.match(result.stdout, markerFound);
  assert.ok(result.stdout.includes(`b/second.png:embedded:ssh ${labHost}\n`), result.stdout);
  assert.ok(result.stdout.includes(`c/third.png:embedded:copied from ${sensitiveFixturePath}\n`), result.stdout);
  assert.doesNotMatch(result.stdout, /first\.png/);
});

test('privacy scan reports a host next to a Unicode line separator', () => {
  // The search ends its lines at a line feed and at nothing else.
  const result = scanFiles({ 'shot.png': png(pngCompressedText(`ssh ${labHost} \u2028 more \u0085 text`)) });

  assert.notEqual(result.status, 0);
  assert.match(result.stdout, markerFound);
  assert.ok(result.stdout.includes(`shot.png:embedded:ssh ${labHost} `), result.stdout);
  assert.doesNotMatch(result.stderr, /Traceback/);
});

test('privacy scan reads text inside PNG files that are not staged yet', () => {
  const result = scanFiles({ 'shot.png': png(pngText(`router ${privateAddress}`)) }, { tracked: false });

  assert.notEqual(result.status, 0);
  assert.match(result.stdout, ipv4Found);
  assert.match(result.stdout, /shot\.png:embedded:/);
});

test('privacy scan reads nothing of a PNG but its text chunks', () => {
  // Pixels, color profiles, EXIF blocks and private chunks spell addresses
  // by chance, and what follows the end of the image is not part of it.
  const result = scanFiles({
    'shot.png': Buffer.concat([
      png(
        pngText('documentation router 192.0.2.1 and [2001:db8::1]'),
        pngChunk('iCCP', addressLikeNoise),
        pngChunk('eXIf', addressLikeNoise),
        pngChunk('prVW', addressLikeNoise),
      ),
      addressLikeNoise,
    ]),
  });

  assert.equal(result.status, 0, result.stderr || result.stdout);
});

test('privacy scan does not read binary files of other kinds', () => {
  // As before this reader: images of other formats, video, fonts and
  // archives are not read for addresses, whatever they hold.
  const riffChunk = (type, body) => Buffer.concat([latin1(type), uint(4, body.length, true), body]);
  const webpBody = Buffer.concat([latin1('WEBP'), riffChunk('XMP ', latin1(`<x>${privateAddress}</x> `))]);
  const mp4Box = (type, body) => Buffer.concat([uint(4, body.length + 8), latin1(type), body]);
  const result = scanFiles({
    'shot.webp': Buffer.concat([latin1('RIFF'), uint(4, webpBody.length, true), webpBody]),
    'clip.mp4': Buffer.concat([
      mp4Box('ftyp', latin1('isom\0\0\0\0isom')),
      mp4Box('moov', mp4Box('udta', mp4Box('name', latin1(`camera at ${privateAddress}`)))),
    ]),
    'font.woff2': Buffer.concat([latin1('wOF2'), Buffer.alloc(44), addressLikeNoise]),
    'font.ttf': Buffer.concat([Buffer.from([0, 1, 0, 0, 0, 12]), addressLikeNoise, latin1(`\0${privateAddress}\0`)]),
    'archive.bin': Buffer.concat([Buffer.from([0x50, 0x4b, 3, 4, 0]), addressLikeNoise]),
  });

  assert.equal(result.status, 0, result.stderr || result.stdout);
});

// Twenty font files of this system, where it has any.
function systemFonts() {
  const fonts = [];
  const pending = ['/System/Library/Fonts', '/usr/share/fonts'].filter((directory) => existsSync(directory));
  while (pending.length > 0 && fonts.length < 20) {
    const directory = pending.shift();
    for (const entry of readdirSync(directory, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const path = `${directory}/${entry.name}`;
      if (entry.isDirectory()) {
        pending.push(path);
      } else if (entry.isFile() && /\.(?:ttf|otf|ttc)$/i.test(entry.name) && statSync(path).size <= 5_000_000) {
        fonts.push(path);
      }
    }
  }
  return fonts.slice(0, 20);
}

test(
  'privacy scan passes the fonts of this system',
  { skip: systemFonts().length === 0 ? 'this system has no font files where they are looked for' : false },
  () => {
    const result = scanFiles(
      Object.fromEntries(systemFonts().map((path, index) => [`font-${index}.${path.split('.').pop()}`, readFileSync(path)])),
    );

    assert.equal(result.status, 0, result.stderr || result.stdout);
  },
);

// Each of these would otherwise pass with text unread, or keep the scan busy.
const stream = deflateSync('synthetic text');
for (const [label, path, contents, said] of [
  [
    // The length would carry the scan past the text chunk behind it.
    'a PNG pixel chunk that says it runs past the end of the file',
    'pixels.png',
    png(pngChunk('IDAT', latin1('xx'), 0xffffffff), pngText(`router ${privateAddress}`)),
    /pixels\.png: a PNG chunk that runs past the end of the file/,
  ],
  [
    'a PNG text chunk that says it runs past the end of the file',
    'text.png',
    png(pngChunk('tEXt', latin1(`note\0router ${privateAddress}`), 0xffffffff)),
    /text\.png: a PNG chunk that runs past the end of the file/,
  ],
  [
    'a PNG file that ends inside a chunk',
    'cut.png',
    png(pngText(`router ${privateAddress}`)).subarray(0, 45),
    /cut\.png: a PNG chunk that runs past the end of the file/,
  ],
  [
    // 64 MiB of spaces deflate to a few kilobytes.
    'compressed PNG text that inflates past the limit',
    'bomb.png',
    png(pngCompressedText('', deflateSync(Buffer.alloc(64 * 1024 * 1024, 0x20)))),
    /bomb\.png: compressed text larger than 8388608 bytes/,
  ],
  [
    'compressed PNG text that does not inflate',
    'corrupt.png',
    png(pngCompressedText('', latin1('not a zlib stream'))),
    /corrupt\.png: compressed text that does not inflate/,
  ],
  [
    'compressed PNG text without its checksum',
    'short.png',
    png(pngCompressedText('', stream.subarray(0, stream.length - 4))),
    /short\.png: compressed text that is cut short/,
  ],
  [
    'compressed international PNG text without its checksum',
    'short-itxt.png',
    png(pngInternationalText('', { stream: stream.subarray(0, stream.length - 4) })),
    /short-itxt\.png: compressed text that is cut short/,
  ],
  [
    'compressed PNG text that stops half-way',
    'half.png',
    png(pngCompressedText('', stream.subarray(0, 6))),
    /half\.png: compressed text that is cut short/,
  ],
  [
    'a compressed international PNG text chunk without its fields',
    'flag.png',
    png(pngChunk('iTXt', latin1('note\0\x01'))),
    /flag\.png: a compressed text chunk that is cut short/,
  ],
  [
    // A second stream behind the first is not text any reader shows.
    'compressed PNG text with a second stream behind it',
    'two.png',
    png(pngCompressedText('', Buffer.concat([stream, deflateSync(`router ${privateAddress}`)]))),
    /two\.png: compressed text with more data behind it/,
  ],
  [
    // A few kilobytes that inflate to a million one-letter pieces.
    'PNG text in more pieces than any real file holds',
    'pieces.png',
    png(pngCompressedText('', deflateSync(Buffer.from('a\0'.repeat(1_000_000))))),
    /pieces\.png: text in more than 100000 pieces/,
  ],
  [
    'more PNG text chunks than any real file holds',
    'chunks.png',
    png(Buffer.concat(Array(200_000).fill(pngChunk('tEXt', latin1('a'))))),
    /chunks\.png: text in more than 100000 pieces/,
  ],
  [
    // Each stream is under the limit; together they are not.
    'more PNG text than any real file holds',
    'streams.png',
    png(...Array(5).fill(pngCompressedText('', deflateSync(Buffer.alloc(3 * 1024 * 1024, 0x61))))),
    /streams\.png: more than 8388608 bytes of text/,
  ],
  [
    'a PNG file larger than it reads',
    'huge.png',
    Buffer.concat([png(), Buffer.alloc(32 * 1024 * 1024)]),
    /huge\.png: a PNG file larger than 33554432 bytes/,
  ],
]) {
  test(`privacy scan refuses ${label}`, () => {
    const result = scanFiles({ [path]: contents });

    assert.notEqual(result.status, 0, `${path} passed the scan`);
    assert.match(result.stdout, said);
    assert.doesNotMatch(result.stderr, /Traceback/);
  });
}

test('privacy scan passes more PNG text than the largest real file held', () => {
  // The most in 23,902 PNG files of one system was 673,702 bytes in 9,414
  // pieces. This is a megabyte in 25,000 lines.
  const result = scanFiles({ 'large.png': png(pngCompressedText('synthetic text of a hundred bytes or so\n'.repeat(25_000))) });

  assert.equal(result.status, 0, result.stderr || result.stdout);
});

test('privacy scan is not held up by a long run of address characters in PNG text', () => {
  // The IPv6 pattern needs time quadratic in the length of such a run.
  const dots = '.'.repeat(512 * 1024);
  let result = scanFiles({ 'dots.png': png(pngText(`${dots}:`)) });
  assert.equal(result.status, 0, result.stderr || result.stdout);

  // An address next to the run is still an address.
  result = scanFiles({ 'dots.png': png(pngText(`${dots}: seen from ${uniqueLocalAddress}`)) });
  assert.notEqual(result.status, 0);
  assert.match(result.stdout, ipv6Found);
});

test('privacy scan spends no more on a line of PNG text under a long path than under a short one', () => {
  // The path used to go to the IPv6 pattern with every line, which took
  // some thirty times the processor time under these directory names. Both
  // runs do the same work now; five times leaves room for an uneven machine.
  const lines = png(pngText(':\n'.repeat(99_990)));
  const short = scanFiles({ 'a/b/c/x.png': lines });
  const long = scanFiles({ [`${Array(3).fill(`${'.'.repeat(200)}a`).join('/')}/x.png`]: lines });

  assert.equal(short.status, 0, short.stderr || short.stdout);
  assert.equal(long.status, 0, long.stderr || long.stdout);
  assert.ok(
    long.cpuSeconds < 5 * short.cpuSeconds,
    `${long.cpuSeconds} s of processor time under the long path, ${short.cpuSeconds} s under the short one`,
  );
});

test('privacy scan checks the name of a PNG file with text once', () => {
  const directory = `[${uniqueLocalAddress}]`;
  const result = scanFiles({ [`${directory}/shot.png`]: png(pngText('one\ntwo: three\nfour: five')) });

  assert.notEqual(result.status, 0);
  assert.match(result.stdout, ipv6Found);
  assert.equal(result.stdout.split(`${directory}/shot.png:embedded:`).length - 1, 1, result.stdout);
});

test('privacy scan does not follow a link out of the repository', () => {
  const outside = mkdtempSync(`${tmpdir()}/prismtty-privacy-outside-`);
  try {
    // A pipe nobody writes to: opening it for reading waits forever.
    execFileSync('mkfifo', [`${outside}/pipe`]);
    writeFileSync(`${outside}/shot.png`, png(pngText(`router ${privateAddress}`)));
    const result = scanFiles({}, { links: { 'pipe.bin': `${outside}/pipe`, 'shot.png': `${outside}/shot.png` } });

    assert.equal(result.status, 0, result.stderr || result.stdout);
  } finally {
    // A scan that did wait on the pipe is let go: opening the other end
    // only works when there is one.
    try {
      closeSync(openSync(`${outside}/pipe`, constants.O_WRONLY | constants.O_NONBLOCK));
    } catch {
      // Nobody was waiting.
    }
    rmSync(outside, { recursive: true, force: true });
  }
});

// Git still lists the tracked path when a directory above the file has been
// replaced by a link, and does not read through it. What the link points at
// is outside the repository.
for (const [label, tracked, replaced] of [
  ['the directory of a PNG', 'assets/shot.png', 'assets'],
  ['a directory two levels above a PNG', 'far/deep/er/shot.png', 'far'],
]) {
  test(`privacy scan does not read through a link in place of ${label}`, () => {
    const outside = mkdtempSync(`${tmpdir()}/prismtty-privacy-outside-`);
    try {
      const image = png(pngCompressedText(`router ${uniqueLocalAddress}`));
      mkdirSync(`${outside}/deep/er`, { recursive: true });
      writeFileSync(`${outside}/shot.png`, image);
      writeFileSync(`${outside}/deep/er/shot.png`, image);
      const result = scanFiles(
        { [tracked]: png() },
        {
          afterStaging: (directory) => {
            rmSync(`${directory}/${replaced}`, { recursive: true });
            symlinkSync(outside, `${directory}/${replaced}`);
          },
        },
      );

      assert.equal(result.status, 0, result.stderr || result.stdout);
      assert.doesNotMatch(result.stdout, /embedded/);
    } finally {
      rmSync(outside, { recursive: true, force: true });
    }
  });
}

test('privacy scan does not wait on a pipe in place of the directory of a PNG', () => {
  let pipe;
  try {
    const result = scanFiles(
      { 'assets/shot.png': png(pngText(`router ${privateAddress}`)) },
      {
        afterStaging: (directory) => {
          rmSync(`${directory}/assets`, { recursive: true });
          pipe = `${directory}/assets`;
          execFileSync('mkfifo', [pipe]);
        },
      },
    );

    assert.equal(result.status, 0, result.stderr || result.stdout);
    assert.doesNotMatch(result.stderr, /Traceback/);
  } finally {
    // A scan that did wait on the pipe is let go.
    try {
      closeSync(openSync(pipe, constants.O_WRONLY | constants.O_NONBLOCK));
    } catch {
      // Nobody was waiting, or the pipe is gone with its directory.
    }
  }
});

test('privacy scan passes over a tracked PNG whose directory is now a file', () => {
  const result = scanFiles(
    { 'assets/shot.png': png(pngText(`router ${privateAddress}`)) },
    {
      afterStaging: (directory) => {
        rmSync(`${directory}/assets`, { recursive: true });
        writeFileSync(`${directory}/assets`, 'synthetic notes\n');
      },
    },
  );

  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.doesNotMatch(result.stderr, /Traceback/);
});

test('privacy scan reads a PNG in nested directories', () => {
  const result = scanFiles({ 'docs/assets/2026/shot.png': png(pngCompressedText(`router ${uniqueLocalAddress}`)) });

  assert.notEqual(result.status, 0);
  assert.match(result.stdout, ipv6Found);
  assert.match(result.stdout, /docs\/assets\/2026\/shot\.png:embedded:/);
});

// macOS regex matching gives up on a line that is not valid UTF-8 when the
// locale is a UTF-8 one, so the scan used to pass such lines unread there.
for (const locale of ['en_US.UTF-8', 'C']) {
  for (const [kind, text, found] of [
    ['a path', sensitiveFixturePath, markerFound],
    ['an IPv4 address', privateAddress, ipv4Found],
    ['an IPv6 address', uniqueLocalAddress, ipv6Found],
  ]) {
    test(`privacy scan reads ${kind} on a line that is not valid UTF-8 in the ${locale} locale`, () => {
      const line = Buffer.concat([Buffer.from([0x63, 0x61, 0x66, 0xe9, 0x20]), latin1(`${text}\n`)]);
      const result = scanFiles({ 'legacy.txt': line }, { locale });

      assert.notEqual(result.status, 0, `${text} passed the scan`);
      assert.match(result.stdout, found);
      assert.match(result.stdout, /legacy\.txt/);
      assert.doesNotMatch(result.stderr, /Traceback/);
    });
  }

  test(`privacy scan reads a path inside a PNG in the ${locale} locale`, () => {
    const result = scanFiles({ 'shot.png': png(pngText(`from ${sensitiveFixturePath}`)) }, { locale });

    assert.notEqual(result.status, 0);
    assert.match(result.stdout, markerFound);
    assert.match(result.stdout, /shot\.png/);
  });
}
