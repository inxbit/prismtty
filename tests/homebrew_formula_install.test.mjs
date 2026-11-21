import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  chmodSync,
  cpSync,
  existsSync,
  lstatSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import test from 'node:test';

const hasRuby = spawnSync('ruby', ['--version']).status === 0;

// Runs the generated formula's install against a scratch Homebrew prefix that
// `prepare` has filled, and returns where things landed.
function install(prepare) {
  const root = mkdtempSync(`${tmpdir()}/prismtty-brew-`);
  const prefix = `${root}/prefix`;
  const staged = `${root}/staged`;
  mkdirSync(`${prefix}/bin`, { recursive: true });
  mkdirSync(`${staged}/profiles`, { recursive: true });
  // The completion files are the real ones: the formula edits them, and an
  // edit that no longer finds its text must fail here, not on a user's machine.
  cpSync('completions', `${staged}/completions`, { recursive: true });
  for (const name of ['prismtty', 'ptty', 'ct']) {
    writeFileSync(`${staged}/${name}`, `#!/bin/sh\necho ${name}\n`, { mode: 0o755 });
  }
  writeFileSync(`${staged}/profiles/example.yml`, 'rules: []\n');
  for (const target of ['darwin-aarch64', 'darwin-x86_64', 'linux-x86_64']) {
    writeFileSync(
      `${root}/prismtty-1.2.3-${target}.tar.gz.sha256`,
      `${'a'.repeat(64)}  prismtty-1.2.3-${target}.tar.gz\n`,
    );
  }
  const generated = spawnSync('bash', ['scripts/generate-homebrew-formula.sh', `${root}/prismtty.rb`, root], {
    encoding: 'utf8',
    env: { ...process.env, PRISMTTY_VERSION: '1.2.3' },
  });
  assert.equal(generated.status, 0, generated.stderr);
  const cleanup = prepare(prefix) ?? (() => {});
  const run = spawnSync('ruby', ['tests/fixtures/formula_stub.rb', `${root}/prismtty.rb`, prefix, staged], {
    encoding: 'utf8',
  });
  cleanup();
  const keg = `${prefix}/Cellar/prismtty/1.2.3`;
  const present = (path) => {
    try {
      lstatSync(`${keg}/${path}`);
      return true;
    } catch {
      return false;
    }
  };
  const text = (path) => readFileSync(`${keg}/${path}`, 'utf8');
  return {
    run,
    present,
    text,
    caveats: run.stdout.match(/^caveats: (.*)$/m)?.[1],
    remove: () => rmSync(root, { recursive: true, force: true }),
  };
}

const ZSH = 'share/zsh/site-functions/_prismtty';
const BASH = 'etc/bash_completion.d/prismtty';
const FISH = 'share/fish/vendor_completions.d/prismtty.fish';

// Nothing else owns `ct`: everything is installed as before.
function assertCtLinked(result, state) {
  assert.equal(result.run.status, 0, `${state}: ${result.run.stderr}`);
  for (const path of ['bin/prismtty', 'bin/ptty', 'bin/ct', 'etc/bash_completion.d/ct', 'share/fish/vendor_completions.d/ct.fish']) {
    assert.ok(result.present(path), `${state}: ${path} is installed`);
  }
  assert.ok(!result.present('libexec/bin/ct'), `${state}: ct is not set aside`);
  assert.equal(result.text(ZSH).split('\n')[0], '#compdef prismtty ptty ct', state);
  assert.match(result.text(BASH), /^\s*complete -F _ct .* ct$/m, state);
  assert.match(result.text(FISH), /^complete -c ct /m, state);
  assert.equal(result.caveats, 'nil', state);
}

// Something else owns `ct`: PrismTTY's stays out of every linked directory,
// and the shared completion files no longer claim the name.
function assertCtSetAside(result, state) {
  assert.equal(result.run.status, 0, `${state}: ${result.run.stderr}`);
  for (const path of ['bin/prismtty', 'bin/ptty', 'libexec/bin/ct', 'etc/bash_completion.d/ptty', 'share/fish/vendor_completions.d/ptty.fish']) {
    assert.ok(result.present(path), `${state}: ${path} is installed`);
  }
  for (const path of ['bin/ct', 'etc/bash_completion.d/ct', 'share/fish/vendor_completions.d/ct.fish']) {
    assert.ok(!result.present(path), `${state}: ${path} would collide and is not installed`);
  }
  assert.equal(result.text(ZSH).split('\n')[0], '#compdef prismtty ptty', state);
  assert.doesNotMatch(result.text(BASH), /complete -F _ct /, state);
  assert.match(result.text(BASH), /^\s*complete -F _ptty .* ptty$/m, state);
  assert.doesNotMatch(result.text(FISH), /^complete -c ct /m, state);
  assert.match(result.text(FISH), /^complete -c ptty /m, state);
  assert.match(result.caveats, /opt\/prismtty\/libexec\/bin/, state);
  assert.match(result.caveats, /brew reinstall inxbit\/tap\/prismtty/, state);
}

test('the formula links ct when nothing else in the prefix owns it', { skip: !hasRuby && 'ruby is not installed' }, () => {
  const states = {
    'no ct in the prefix': () => {},
    'a link into an older PrismTTY keg': (prefix) => {
      mkdirSync(`${prefix}/Cellar/prismtty/1.0.0/bin`, { recursive: true });
      writeFileSync(`${prefix}/Cellar/prismtty/1.0.0/bin/ct`, '');
      symlinkSync('../Cellar/prismtty/1.0.0/bin/ct', `${prefix}/bin/ct`);
    },
    'a dangling link': (prefix) => {
      symlinkSync('../Cellar/gone/1.0/bin/ct', `${prefix}/bin/ct`);
    },
  };
  for (const [state, prepare] of Object.entries(states)) {
    const result = install(prepare);
    try {
      assertCtLinked(result, state);
    } finally {
      result.remove();
    }
  }
});

// With another `ct` in the prefix, Homebrew's link step fails on it and then
// unlinks the whole formula: no prismtty and no ptty either.
test('the formula sets ct aside when something else in the prefix owns it', { skip: !hasRuby && 'ruby is not installed' }, () => {
  const states = {
    'a link into another formula': (prefix) => {
      mkdirSync(`${prefix}/Cellar/chart-testing/3.0.0/bin`, { recursive: true });
      writeFileSync(`${prefix}/Cellar/chart-testing/3.0.0/bin/ct`, '');
      symlinkSync('../Cellar/chart-testing/3.0.0/bin/ct', `${prefix}/bin/ct`);
    },
    'a plain file': (prefix) => {
      writeFileSync(`${prefix}/bin/ct`, '#!/bin/sh\n');
    },
    'a link the install cannot follow': (prefix) => {
      mkdirSync(`${prefix}/locked/bin`, { recursive: true });
      writeFileSync(`${prefix}/locked/bin/ct`, '');
      symlinkSync('../locked/bin/ct', `${prefix}/bin/ct`);
      chmodSync(`${prefix}/locked`, 0o000);
      return () => chmodSync(`${prefix}/locked`, 0o755);
    },
  };
  for (const [state, prepare] of Object.entries(states)) {
    const result = install(prepare);
    try {
      assertCtSetAside(result, state);
    } finally {
      result.remove();
    }
  }
});
