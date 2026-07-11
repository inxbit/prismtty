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
