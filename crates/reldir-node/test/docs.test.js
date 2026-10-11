'use strict';
// The Node page of the documentation is executed: every ```js block of
// docs/node.md runs, in order, in one async scope, in a folder holding the
// database the examples use (`data/`, copied from
// crates/reldir/tests/docs/snippets/). Blocks that state a signature rather
// than run are written as ```text and are not run.

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const repository = path.resolve(__dirname, '..', '..', '..');

test('every Node example in the documentation runs', async () => {
  const page = fs.readFileSync(path.join(repository, 'docs', 'node.md'), 'utf8');
  const examples = [...page.matchAll(/^```js\n([\s\S]*?)^```/gm)].map((match) => match[1]);
  assert.ok(examples.length >= 5, 'the page shows its interface working');
  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), 'reldir-node-docs-'));
  fs.cpSync(path.join(repository, 'crates/reldir/tests/docs/snippets/data'), path.join(scratch, 'data'), {
    recursive: true,
  });
  // The examples require the published name; here it is this package.
  const local = (name) => (name === '@reldir/reldir' ? require('..') : require(name));
  const body = examples.join('\n');
  const AsyncFunction = Object.getPrototypeOf(async () => {}).constructor;
  const run = new AsyncFunction('require', 'console', body);
  const previous = process.cwd();
  process.chdir(scratch);
  try {
    await run(local, { log: () => {} });
  } finally {
    process.chdir(previous);
  }
});
