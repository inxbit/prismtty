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

// zsh completion only runs inside the line editor, so each case types a line
// into an interactive zsh on a pseudo-terminal (zsh's own zpty module),
// presses TAB, and reads back the line and the candidates that were offered.
const probe = spawnSync('zsh', ['-f', '-c', 'zmodload zsh/zpty zsh/zselect'], { encoding: 'utf8' });
const zshAvailable = !probe.error && probe.status === 0;
// The macOS runners always have zsh, so the cases must run there. The Ubuntu
// runner image ships without it; there the cases are skipped, not failed.
const skip = !zshAvailable && process.platform !== 'darwin'
  && 'zsh with the zpty module is not installed on this host';

// Sourced by the interactive zsh: completion from the checked-in file only,
// a fixed directory and PATH, and a key that completes and then reports.
const interactiveSetup = `PS1='%% '
cd $ZC_DIRECTORY
path=($ZC_PATH)
fpath=($ZC_FUNCTIONS $fpath)
autoload -Uz compinit && compinit -u -D
zstyle ':completion:*' completer _complete
compadd() {
  if [[ \${@[1,(i)(-|--)]} == *-(O|A|D)\\ * ]]; then
    builtin compadd "$@"
    return
  fi
  local -a offered
  builtin compadd -A offered "$@"
  (( $#offered )) && print -rl -- $offered >> $ZC_WORK/candidates
  builtin compadd "$@"
}
complete-and-report() {
  zle complete-word
  print -r -- "$BUFFER" > $ZC_WORK/buffer
  : > $ZC_WORK/done
  exit
}
zle -N complete-and-report
bindkey '^X^Y' complete-and-report
: > $ZC_WORK/ready
`;

// Starts the interactive zsh and waits on marker files, never on a fixed delay.
const driver = `zmodload zsh/zpty zsh/zselect || exit 3
typeset -F SECONDS
local work=$1 line=$2 unread
local -F deadline=$(( SECONDS + 30 ))
wait_for() {
  until [[ -e $work/$1 ]]; do
    zpty -r interactive unread 2>/dev/null
    if (( SECONDS > deadline )); then
      zpty -d interactive
      exit 4
    fi
    zselect -t 2
  done
}
zpty -b interactive 'zsh -f -i'
zpty -w interactive "source \${(q)work}/setup.zsh"
wait_for ready
zpty -w -n interactive "$line"$'\\x18\\x19'
wait_for done
zpty -d interactive
`;

// A wrapped command with a completion of its own, which also records the
// words it was handed.
const wrappedCommandCompletion = `#compdef fakecmd
print -r -- "\${(j: :)words}" > $ZC_WORK/wrapped-words
_arguments '--fake-flag[a flag of the wrapped command]' '*:word:(alpha beta)'
`;

function pressTab(line) {
  const root = mkdtempSync(`${tmpdir()}/prismtty-zsh-`);
  try {
    for (const path of ['functions', 'bin', 'home', 'directory/subdir', 'work']) {
      mkdirSync(`${root}/${path}`, { recursive: true });
    }
    copyFileSync('completions/_prismtty', `${root}/functions/_prismtty`);
    writeFileSync(`${root}/functions/_fakecmd`, wrappedCommandCompletion);
    for (const executable of ['bin/fakecmd', 'directory/local-script.sh', 'directory/subdir/tool']) {
      writeFileSync(`${root}/${executable}`, '#!/bin/sh\n');
      chmodSync(`${root}/${executable}`, 0o755);
    }
    for (const filename of ['config.yml', 'notes.txt']) {
      writeFileSync(`${root}/directory/${filename}`, 'fixture\n');
    }
    writeFileSync(`${root}/work/setup.zsh`, interactiveSetup);
    writeFileSync(`${root}/work/driver.zsh`, driver);

    const result = spawnSync('zsh', ['-f', `${root}/work/driver.zsh`, `${root}/work`, line], {
      encoding: 'utf8',
      env: {
        PATH: process.env.PATH,
        HOME: `${root}/home`,
        TERM: 'dumb',
        LANG: 'en_US.UTF-8',
        ZC_WORK: `${root}/work`,
        ZC_DIRECTORY: `${root}/directory`,
        ZC_PATH: `${root}/bin`,
        ZC_FUNCTIONS: `${root}/functions`,
      },
    });
    assert.equal(result.status, 0, `zsh did not complete "${line}": ${result.stderr}`);
    const read = (name) =>
      existsSync(`${root}/work/${name}`) ? readFileSync(`${root}/work/${name}`, 'utf8') : '';
    return {
      line: read('buffer').replace(/\n$/, ''),
      candidates: [...new Set(read('candidates').split('\n').filter(Boolean))].sort(),
      wrappedWords: read('wrapped-words').replace(/\n$/, ''),
    };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test('zsh completes the profiles subcommands right after the profiles command', { skip }, () => {
  // The subcommands were offered one word late: after `profiles` zsh offered
  // `profiles` again, and `list` and the others only after a second word.
  for (const command of ['prismtty', 'ptty', 'ct']) {
    assert.equal(pressTab(`${command} profiles sh`).line, `${command} profiles show `);
  }
  assert.deepEqual(pressTab('prismtty profiles ').candidates, ['list', 'show', 'test', 'validate']);
  assert.equal(pressTab("prismtty 'profiles' sh").line, "prismtty 'profiles' show ");
  assert.equal(pressTab('prismtty -b profiles val').line, 'prismtty -b profiles validate ');
});

test('zsh offers nothing of PrismTTY once the wrapped command has started', { skip }, () => {
  assert.equal(pressTab('prismtty fakecmd profi').line, 'prismtty fakecmd profi');
  assert.equal(pressTab('prismtty -- profiles sh').line, 'prismtty -- profiles sh');
  assert.equal(pressTab('prismtty -b -- profiles sh').line, 'prismtty -b -- profiles sh');

  // The wrapped command's own completion takes over, with its words intact
  // even where they are spelled like PrismTTY options.
  for (const command of ['prismtty', 'ptty', 'ct']) {
    const flags = pressTab(`${command} fakecmd -`);
    assert.equal(flags.line, `${command} fakecmd --fake-flag `);
    assert.deepEqual(flags.candidates, ['--fake-flag']);
  }
  assert.equal(pressTab('prismtty -- fakecmd al').line, 'prismtty -- fakecmd alpha ');
  assert.equal(pressTab('prismtty fakecmd -c x al').wrappedWords, 'fakecmd -c x al');
  assert.equal(pressTab('prismtty -p cisco fakecmd -p x al').wrappedWords, 'fakecmd -p x al');
});

test('zsh still offers PrismTTY options in front of the wrapped command', { skip }, () => {
  assert.equal(pressTab('prismtty --sa').line, 'prismtty --sanitize ');
  assert.equal(pressTab('prismtty -p profiles --sa').line, 'prismtty -p profiles --sanitize ');
  assert.equal(pressTab('prismtty --config profiles --sa').line, 'prismtty --config profiles --sanitize ');
});

test('zsh keeps filename completion wherever a file is a valid argument', { skip }, () => {
  assert.equal(pressTab('prismtty -c con').line, 'prismtty -c config.yml ');
  assert.equal(pressTab('prismtty --trace-io=no').line, 'prismtty --trace-io=notes.txt ');
  assert.equal(pressTab('prismtty profiles validate con').line, 'prismtty profiles validate config.yml ');
  assert.equal(pressTab('prismtty profiles test cisco no').line, 'prismtty profiles test cisco notes.txt ');
  // The first word is a command to wrap: a path or an executable completes.
  assert.equal(pressTab('prismtty ./loc').line, 'prismtty ./local-script.sh ');
  assert.equal(pressTab('prismtty loc').line, 'prismtty local-script.sh ');
  assert.equal(pressTab('prismtty sub').line, 'prismtty subdir/');
  assert.equal(pressTab('prismtty subdir/t').line, 'prismtty subdir/tool ');
  assert.equal(pressTab('prismtty -- loc').line, 'prismtty -- local-script.sh ');
});
