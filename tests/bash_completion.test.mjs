import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import {
  mkdtempSync,
  mkdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import test from 'node:test';

const completionPath = resolve('completions/prismtty.bash');

test('Bash filename completion preserves unusual filenames for every alias', () => {
  const directory = mkdtempSync(`${tmpdir()}/prismtty-completion-`);
  const filenames = [
    '-leading.yml',
    'glob[*]?.yml',
    'space name.yml',
    'tab\tname.yml',
    'unicode-雪.yml',
  ];

  try {
    mkdirSync(`${directory}/ordinary-dir`);
    for (const filename of filenames) {
      writeFileSync(`${directory}/${filename}`, 'profile: test\n');
    }

    for (const command of ['prismtty', 'ptty', 'ct']) {
      const output = execFileSync(
        'bash',
        [
          '-c',
          `source "$1"
           cd "$2"
           COMP_WORDS=("$3" "--config" "")
           COMP_CWORD=2
           completion_function="_$3"
           "$completion_function" "$3" "" "--config"
           printf '%s\\0' "\${COMPREPLY[@]}"`,
          'bash',
          completionPath,
          directory,
          command,
        ],
        { encoding: 'buffer' },
      );
      const replies = output
        .toString('utf8')
        .split('\0')
        .filter(Boolean)
        .sort();

      assert.deepEqual(replies, [...filenames, 'ordinary-dir'].sort(), command);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

const commandNames = ['prismtty', 'ptty', 'ct'];

// Calls the completion function the way Bash does for `command words...`,
// with the cursor on the last word, in a directory with known files.
function withCompletionDirectory(run) {
  const directory = mkdtempSync(`${tmpdir()}/prismtty-completion-`);
  try {
    mkdirSync(`${directory}/subdir`);
    for (const filename of ['config.yml', 'notes.txt', 'local-script.sh']) {
      writeFileSync(`${directory}/${filename}`, 'fixture\n');
    }
    const complete = (command, words) => {
      const output = execFileSync(
        'bash',
        [
          '-c',
          `source "$1"
           cd "$2"
           shift 2
           COMP_WORDS=("$@")
           COMP_CWORD=$(( $# - 1 ))
           "_$1" "$1" "\${COMP_WORDS[COMP_CWORD]}" "\${COMP_WORDS[COMP_CWORD-1]}"
           printf '%s\\0' "\${COMPREPLY[@]}"`,
          'bash',
          completionPath,
          directory,
          command,
          ...words,
        ],
        { encoding: 'utf8' },
      );
      return output.split('\0').filter(Boolean);
    };
    run(complete);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

test('Bash offers nothing of PrismTTY once the wrapped command has started', () => {
  // An empty reply hands over to the `-o default` filename completion. The
  // words are given both as Bash 3 passes them and as Bash 4 and later split
  // them at `=`.
  const wrapped = [
    ['--', 'ssh', ''],
    ['ssh', ''],
    ['ssh', '--sa'],
    ['cat', 'pro'],
    ['--', 'profiles', 'sh'],
    ['-pcisco', 'ssh', '--sa'],
    ['-p', 'cisco', 'ssh', '--sa'],
    ['-Rp', 'generic', 'ssh', '--sa'],
    ['--profile=cisco', 'ssh', '--sa'],
    ['--profile', '=', 'cisco', 'ssh', '--sa'],
    ['--profile', 'cisco', '--', 'ssh', 'host', '-'],
    ['>', 'log', 'ssh', '--sa'],
    ['ssh', 'host', '>', 'log', '--sa'],
    ['--', 'ssh', 'host', '2', '>', '/dev/null', '-'],
  ];
  withCompletionDirectory((complete) => {
    for (const command of commandNames) {
      for (const words of wrapped) {
        assert.deepEqual(complete(command, words), [], `${command} ${words.join(' ')}`);
      }
    }
  });
});

test('Bash still offers PrismTTY options in front of the wrapped command', () => {
  // An option value, a split value and a redirection are not the wrapped
  // command, whatever they are called. Bash passes `>log` as `>`, `log` and
  // `2>/dev/null` as `2`, `>`, `/dev/null`.
  const own = [
    ['--sa'],
    ['-p', 'profiles', '--sa'],
    ['--config', 'profiles', '--sa'],
    ['-pcisco', '--sa'],
    ['-Rp', 'generic', '--sa'],
    ['--profile=cisco', '--sa'],
    ['--profile', '=', 'cisco', '--sa'],
    ['--trace-io', 'trace:1.log', '--sa'],
    ['--trace-io', 'trace', ':', '1.log', '--sa'],
    ['>', 'log', '--sa'],
    ['2', '>', '/dev/null', '--sa'],
    ['<', 'notes.txt', '-p', 'cisco', '--sa'],
  ];
  withCompletionDirectory((complete) => {
    for (const command of commandNames) {
      for (const words of own) {
        assert.deepEqual(complete(command, words), ['--sanitize'], `${command} ${words.join(' ')}`);
      }
    }
  });
});

test('Bash completes the profiles subcommands for the profiles command only', () => {
  withCompletionDirectory((complete) => {
    for (const command of commandNames) {
      for (const words of [['profiles', 'sh'], ['-b', 'profiles', 'sh'], ['>>', 'log', 'profiles', 'sh']]) {
        assert.deepEqual(complete(command, words), ['show'], `${command} ${words.join(' ')}`);
      }
      assert.deepEqual(
        complete(command, ['profiles', '']).sort(),
        ['list', 'show', 'test', 'validate'],
        `${command} profiles`,
      );
    }
  });
});

test('Bash keeps filename completion wherever a file is a valid argument', () => {
  withCompletionDirectory((complete) => {
    for (const command of commandNames) {
      assert.deepEqual(complete(command, ['-c', 'con']), ['config.yml'], command);
      assert.deepEqual(complete(command, ['--config', 'con']), ['config.yml'], command);
      assert.deepEqual(complete(command, ['--trace-io', 'no']), ['notes.txt'], command);
      // These reply with nothing, which leaves the word to the default
      // filename completion the command is registered with.
      for (const words of [
        ['profiles', 'validate', 'con'],
        ['profiles', 'test', 'cisco', 'no'],
        ['./loc'],
        ['loc'],
        ['sub'],
      ]) {
        assert.deepEqual(complete(command, words), [], `${command} ${words.join(' ')}`);
      }
    }
  });
  const registered = execFileSync(
    'bash',
    ['-c', 'source "$1"; complete -p prismtty ptty ct', 'bash', completionPath],
    { encoding: 'utf8' },
  );
  for (const command of commandNames) {
    assert.match(
      registered,
      new RegExp(`^complete .*-o default .*-F _${command} ${command}$`, 'm'),
      `${command} is registered with default filename completion`,
    );
  }
});
